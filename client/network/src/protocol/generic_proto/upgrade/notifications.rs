// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

/// Notifications protocol.
///
/// The Substrate notifications protocol consists in the following:
///
/// - Node A opens a substream to node B.
/// - If node B accepts the substream, it sends back a message which contains some
///   protocol-specific higher-level logic. This message is prefixed with a variable-length
///   integer message length. This message can be empty, in which case `0` is sent. Afterwards,
///   the sending side of B is closed.
/// - If instead the node refuses the connection (which typically happens because no empty slot
///   is available), then it immediately closes the substream after the multistream-select
///   negotiation.
/// - Node A can then send notifications to B, prefixed with a variable-length integer indicating
///   the length of the message.
/// - Node A closes its writing side if it doesn't want the notifications substream anymore.
///
/// Notification substreams are unidirectional. If A opens a substream with B, then B is
/// encouraged but not required to open a substream to A as well.
///

use bytes::BytesMut;
use futures::prelude::*;
use futures_codec::Framed;
use libp2p::core::{UpgradeInfo, InboundUpgrade, OutboundUpgrade, upgrade};
use log::error;
use std::{borrow::Cow, collections::VecDeque, io, iter, mem, pin::Pin, task::{Context, Poll}};
use unsigned_varint::codec::UviBytes;

/// Upgrade that accepts a substream, sends back a status message, then becomes a unidirectional
/// stream of messages.
#[derive(Debug, Clone)]
pub struct NotificationsIn {
	/// Protocol name to use when negotiating the substream.
	protocol_name: Cow<'static, [u8]>,
}

/// Upgrade that opens a substream, waits for the remote to accept by sending back a status
/// message, then becomes a unidirectional sink of data.
#[derive(Debug, Clone)]
pub struct NotificationsOut {
	/// Protocol name to use when negotiating the substream.
	protocol_name: Cow<'static, [u8]>,
}

/// A substream for incoming notification messages.
///
/// When creating, this struct starts in a state in which we must first send back a handshake
/// message to the remote. No message will come before this has been done.
#[pin_project::pin_project]
pub struct NotificationsInSubstream<TSubstream> {
	#[pin]
	socket: Framed<TSubstream, UviBytes<VecDeque<u8>>>,
	handshake: NotificationsInSubstreamHandshake,
}

/// State of the handshake sending back process.
enum NotificationsInSubstreamHandshake {
	/// Waiting for the user to give us the handshake message.
	NotSent,
	/// User gave us the handshake message. Trying to push it in the socket.
	PendingSend(Vec<u8>),
	/// Handshake message was pushed in the socket. Still need to flush.
	Close,
	/// Handshake message successfully sent.
	Sent,
}

/// A substream for outgoing notification messages.
#[pin_project::pin_project]
pub struct NotificationsOutSubstream<TSubstream> {
	/// Substream where to send messages.
	#[pin]
	socket: Framed<TSubstream, UviBytes<VecDeque<u8>>>,
	/// Queue of messages waiting to be sent.
	messages_queue: VecDeque<VecDeque<u8>>,
	/// If true, we need to flush `socket`.
	need_flush: bool,
}

impl NotificationsIn {
	/// Builds a new potential upgrade.
	pub fn new(proto_name: impl Into<Cow<'static, [u8]>>) -> Self {
		NotificationsIn {
			protocol_name: proto_name.into(),
		}
	}

	/// Returns the name of the protocol that we accept.
	pub fn protocol_name(&self) -> &[u8] {
		&self.protocol_name
	}
}

impl UpgradeInfo for NotificationsIn {
	type Info = Cow<'static, [u8]>;
	type InfoIter = iter::Once<Self::Info>;

	fn protocol_info(&self) -> Self::InfoIter {
		iter::once(self.protocol_name.clone())
	}
}

impl<TSubstream> InboundUpgrade<TSubstream> for NotificationsIn
where TSubstream: AsyncRead + AsyncWrite + 'static,
{
	type Output = NotificationsInSubstream<TSubstream>;
	type Future = future::Ready<Result<Self::Output, Self::Error>>;
	type Error = upgrade::ReadOneError;

	fn upgrade_inbound(
		self,
		socket: TSubstream,
		_: Self::Info,
	) -> Self::Future {
		future::ok(NotificationsInSubstream {
			socket: Framed::new(socket, UviBytes::default()),
			handshake: NotificationsInSubstreamHandshake::NotSent,
		})
	}
}

impl<TSubstream> NotificationsInSubstream<TSubstream>
where TSubstream: AsyncRead + AsyncWrite,
{
	/// Sends the handshake in order to inform the remote that we accept the substream.
	// TODO: doesn't seem to work if `message` is empty
	pub fn send_handshake(&mut self, message: impl Into<Vec<u8>>) {
		match self.handshake {
			NotificationsInSubstreamHandshake::NotSent => {}
			_ => {
				error!(target: "sub-libp2p", "Tried to send handshake twice");
				return;
			}
		}

		self.handshake = NotificationsInSubstreamHandshake::PendingSend(message.into());
	}
}

impl<TSubstream> Stream for NotificationsInSubstream<TSubstream>
where TSubstream: AsyncRead + AsyncWrite + Unpin,
{
	type Item = Result<BytesMut, io::Error>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
		let mut this = self.project();

		// This `Stream` implementation first tries to send back the handshake if necessary.
		loop {
			match mem::replace(this.handshake, NotificationsInSubstreamHandshake::Sent) {
				NotificationsInSubstreamHandshake::Sent =>
					return Stream::poll_next(this.socket.as_mut(), cx),
				NotificationsInSubstreamHandshake::NotSent =>
					return Poll::Pending,
				NotificationsInSubstreamHandshake::PendingSend(msg) =>
					match Sink::poll_ready(this.socket.as_mut(), cx) {
						Poll::Ready(_) => {
							*this.handshake = NotificationsInSubstreamHandshake::Close;
							match Sink::start_send(this.socket.as_mut(), msg.into_iter().collect()) { // TODO: cloning
								Ok(()) => {},
								Err(err) => return Poll::Ready(Some(Err(err))),
							}
						},
						Poll::Pending =>
							*this.handshake = NotificationsInSubstreamHandshake::PendingSend(msg),
					},
				NotificationsInSubstreamHandshake::Close =>
					match Sink::poll_close(this.socket.as_mut(), cx)? {
						Poll::Ready(()) =>
							*this.handshake = NotificationsInSubstreamHandshake::Sent,
						Poll::Pending =>
							*this.handshake = NotificationsInSubstreamHandshake::Close,
					},
			}
		}
	}
}

impl NotificationsOut {
	/// Builds a new potential upgrade.
	pub fn new(proto_name: impl Into<Cow<'static, [u8]>>) -> Self {
		NotificationsOut {
			protocol_name: proto_name.into(),
		}
	}
}

impl UpgradeInfo for NotificationsOut {
	type Info = Cow<'static, [u8]>;
	type InfoIter = iter::Once<Self::Info>;

	fn protocol_info(&self) -> Self::InfoIter {
		iter::once(self.protocol_name.clone())
	}
}

impl<TSubstream> OutboundUpgrade<TSubstream> for NotificationsOut
where TSubstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
	type Output = (Vec<u8>, NotificationsOutSubstream<TSubstream>);
	type Future = Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send>>;
	type Error = upgrade::ReadOneError;

	fn upgrade_outbound(
		self,
		mut socket: TSubstream,
		proto_name: Self::Info,
	) -> Self::Future {
		Box::pin(async move {
			let handshake = upgrade::read_one(&mut socket, 1024).await?;
			Ok((handshake, NotificationsOutSubstream {
				socket: Framed::new(socket, UviBytes::default()),
				messages_queue: VecDeque::new(),
				need_flush: false,
			}))
		})
	}
}

impl<TSubstream> NotificationsOutSubstream<TSubstream>
where TSubstream: AsyncRead + AsyncWrite + Unpin,
{
	/// Pushes a message to the queue of messages.
	pub fn push_message(&mut self, message: impl Into<VecDeque<u8>>) {
		// TODO: limit the size of the queue
		self.messages_queue.push_back(message.into());
	}

	/// Processes the substream.
	pub fn process(self: Pin<&mut Self>, cx: &mut Context) -> Result<(), io::Error> {
		let mut this = self.project();

		while !this.messages_queue.is_empty() {
			match Sink::poll_ready(this.socket.as_mut(), cx) {
				Poll::Ready(Err(err)) => return Err(err),
				Poll::Ready(Ok(())) => {
					let msg = this.messages_queue.pop_front()
						.expect("checked for !is_empty above; qed");
					Sink::start_send(this.socket.as_mut(), msg)?;
					*this.need_flush = true;
				},
				Poll::Pending => return Ok(()),
			}
		}

		if *this.need_flush {
			match Sink::poll_flush(this.socket.as_mut(), cx) {
				Poll::Ready(Err(err)) => return Err(err),
				Poll::Ready(Ok(())) => *this.need_flush = false,
				Poll::Pending => {},
			}
		}

		Ok(())
	}
}
