//! Server-initiated RMA against a vdma or valkeylargeobj server.
//!
//! RESP control channel. [`Dialect`] picks the vocabulary.
//!
//! On `efa-direct` the client is a passive target. The transfer needs no local completion
//! queue and no progress polling. The completion arrives via RESP response.

use crate::dma::command::{self, DmaCommand, DmaGetOptions, DmaSetOptions, TransferReply};
use crate::dma::{
    Advertisement, Dialect, DmaBuffer, DmaError, DmaFabric, RegionWindow, TransferReceipt,
};
use resp_proto::{Request, Value};

use ringline_redis::Client;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Resp(#[from] ringline_redis::Error),
    #[error("dma error: {0}")]
    Dma(#[from] DmaError),
}

/// A RESP connection paired with its fabric slot.
pub(crate) struct DmaClient {
    client: Client,
    fabric: DmaFabric,
    buffer: DmaBuffer,
    dialect: Dialect,
    checksum: bool,
}

impl DmaClient {
    /// Hello, insert every server address, and register a `capacity`-byte buffer.
    ///
    /// Under [`Dialect::LargeObj`] the hello binds this fabric's address to the connection, and a
    /// second hello after one has succeeded is refused. The connection sends `BLOB.RDMA_HELLO`,
    /// and `BLOB.HELLO` only if that is answered with an error; it speaks
    /// [`Dialect::LargeObjRdma`] if the first succeeds. See [`negotiate`].
    pub(crate) async fn connect(
        mut client: Client,
        fabric: DmaFabric,
        capacity: usize,
        dialect: Dialect,
    ) -> Result<Self, Error> {
        let mut sender = Hello {
            client: &mut client,
            local_address: fabric.local_address(),
        };
        let (dialect, addresses) = negotiate(dialect, &mut sender).await?;
        for address in addresses {
            fabric.insert_peer(&address).map_err(Error::Dma)?;
        }
        let buffer = fabric.register(vec![0u8; capacity]).map_err(Error::Dma)?;
        Ok(Self {
            client,
            fabric,
            buffer,
            dialect,
            checksum: false,
        })
    }

    /// Verify the CRC-32c the server returns with a read, asking for one where it needs asking. Whether a
    /// write carries one is the caller's choice; see [`Self::set_registered`].
    pub(crate) fn with_checksum(mut self, enabled: bool) -> Self {
        self.checksum = enabled;
        self
    }

    /// `DMA.GET`: the server writes the value at `key` into the registered buffer.
    ///
    /// `None` if the key is absent. The value is the first `bytes_written` bytes of the buffer.
    pub(crate) async fn get(&mut self, key: &[u8]) -> Result<Option<TransferReceipt>, Error> {
        let mut options = DmaGetOptions::default();
        if self.checksum {
            options = options.with_checksum();
        }
        let command = command::get(
            self.dialect,
            self.buffer.advertisement(),
            key,
            self.buffer.capacity(),
            &options,
        )?;
        let receipt = self.transfer(&command).await?.receipt()?;
        if let Some(receipt) = receipt.as_ref() {
            // verify count from the server
            if self.buffer.capacity() < receipt.bytes_written {
                return Err(Error::Dma(DmaError::Integrity(format!(
                    "server reports {} bytes into a {}-byte buffer",
                    receipt.bytes_written,
                    self.buffer.capacity()
                ))));
            }
            self.verify(receipt)?;
        }
        Ok(receipt)
    }

    /// A write from `window`, a region registered on this fabric, with no staging copy.
    ///
    /// `checksum` is the caller's to compute. `None` skips checksum even when
    /// [`Self::with_checksum`] is on.
    pub(crate) async fn set_registered(
        &mut self,
        key: &[u8],
        window: &RegionWindow,
        checksum: Option<u32>,
    ) -> Result<TransferReceipt, Error> {
        let command = set_command(
            self.dialect,
            window.advertisement(),
            key,
            window.length(),
            checksum,
        )?;
        self.commit(&command, window.length()).await
    }

    /// Plain `DEL` on the control connection. Returns true if the key existed.
    pub(crate) async fn delete(&mut self, key: &[u8]) -> Result<bool, Error> {
        Ok(self.client.del(key).await? == 1)
    }

    /// Run a write and confirm the server took all `length` bytes.
    async fn commit(
        &mut self,
        command: &DmaCommand,
        length: usize,
    ) -> Result<TransferReceipt, Error> {
        Ok(self.transfer(command).await?.write_receipt(length)?)
    }

    /// Send a transfer command and destructure its reply, driving progress across the round trip.
    async fn transfer(&mut self, command: &DmaCommand) -> Result<TransferReply, Error> {
        let _progress = self.fabric.drive_progress();
        let reply = self.client.cmd(&request(command)).await?;
        Ok(parse(reply)?)
    }

    /// Check the server's CRC-32c against the bytes in the buffer, when verification is on.
    fn verify(&mut self, receipt: &TransferReceipt) -> Result<(), Error> {
        let (true, Some(expected)) = (self.checksum, receipt.checksum) else {
            return Ok(());
        };
        let Some(landed) = self.buffer.as_host() else {
            return Ok(()); // a dmabuf region has no host mapping to hash
        };
        let found = crate::dma::checksum(&landed[..receipt.bytes_written]);
        if found != expected {
            return Err(Error::Dma(DmaError::Integrity(format!(
                "checksum {found:#010x} does not match the server's {expected:#010x}"
            ))));
        }
        Ok(())
    }
}

/// A write of an advertised window, with the checksum attached when one was computed.
fn set_command(
    dialect: Dialect,
    advertisement: &Advertisement,
    key: &[u8],
    length: usize,
    checksum: Option<u32>,
) -> Result<DmaCommand, DmaError> {
    let mut options = DmaSetOptions::default();
    if let Some(checksum) = checksum {
        options = options.with_checksum(checksum);
    }
    command::set(dialect, advertisement, key, length, &options)
}

/// Borrow a [`DmaCommand`]'s owned arguments as a RESP request.
fn request(command: &DmaCommand) -> Request<'_> {
    let mut request = Request::cmd(command.name().as_bytes());
    for argument in command.arguments() {
        request = request.arg(argument);
    }
    request
}

/// Sends one hello under a given dialect's names.
trait SendHello {
    async fn send_hello(&mut self, dialect: Dialect) -> Result<Vec<Vec<u8>>, Error>;
}

/// [`SendHello`] over a connection.
struct Hello<'a> {
    client: &'a mut Client,
    local_address: &'a [u8],
}

impl SendHello for Hello<'_> {
    async fn send_hello(&mut self, dialect: Dialect) -> Result<Vec<Vec<u8>>, Error> {
        hello(self.client, dialect, self.local_address).await
    }
}

/// The hello under the names the server has, and the dialect that succeeded.
///
/// For [`Dialect::LargeObj`]: `BLOB.RDMA_HELLO`, and if the server answers it with an error,
/// `BLOB.HELLO`. A server without a command answers with an error and keeps no state, so the
/// second attempt starts clean. If both fail, the error carries both replies. Any other failure,
/// such as a closed connection, ends the attempt.
async fn negotiate(
    dialect: Dialect,
    sender: &mut impl SendHello,
) -> Result<(Dialect, Vec<Vec<u8>>), Error> {
    if dialect != Dialect::LargeObj {
        return Ok((dialect, sender.send_hello(dialect).await?));
    }
    let first = match sender.send_hello(Dialect::LargeObjRdma).await {
        Ok(addresses) => return Ok((Dialect::LargeObjRdma, addresses)),
        Err(Error::Resp(ringline_redis::Error::Redis(message))) => message,
        Err(error) => return Err(error),
    };
    match sender.send_hello(Dialect::LargeObj).await {
        Ok(addresses) => Ok((Dialect::LargeObj, addresses)),
        Err(Error::Resp(ringline_redis::Error::Redis(second))) => Err(Error::Resp(
            ringline_redis::Error::Redis(format!("BLOB.RDMA_HELLO: {first}; BLOB.HELLO: {second}")),
        )),
        Err(error) => Err(error),
    }
}

/// The hello, decoded into every fabric address the server may initiate from.
async fn hello(
    client: &mut Client,
    dialect: Dialect,
    local_address: &[u8],
) -> Result<Vec<Vec<u8>>, Error> {
    let reply = client
        .cmd(&request(&command::hello(dialect, local_address)))
        .await?;
    let Value::Array(items) = reply else {
        return Err(Error::Dma(DmaError::Protocol(format!(
            "hello: expected an array, got {reply:?}"
        ))));
    };
    if items.is_empty() {
        return Err(Error::Dma(DmaError::Protocol(
            "hello returned no addresses".into(),
        )));
    }
    items
        .iter()
        .map(|item| match item {
            Value::BulkString(encoded) | Value::SimpleString(encoded) => {
                crate::dma::decode_hex(encoded).map_err(|error| Error::Dma(error.into()))
            }
            other => Err(Error::Dma(DmaError::Protocol(format!(
                "hello: expected an address, got {other:?}"
            )))),
        })
        .collect()
}

/// Destructure a RESP reply into a [`TransferReply`].
fn parse(reply: Value) -> Result<TransferReply, DmaError> {
    Ok(match reply {
        Value::Null => TransferReply::Missing,
        Value::Integer(bytes) => TransferReply::Bytes(bytes),
        Value::SimpleString(status) if status.as_ref() == b"OK" => TransferReply::Acknowledged,
        Value::Array(items) => match items.as_slice() {
            [Value::Integer(bytes), Value::Integer(checksum)] => TransferReply::BytesAndChecksum {
                bytes: *bytes,
                checksum: *checksum,
            },
            other => {
                return Err(DmaError::Protocol(format!(
                    "expected [bytes, checksum], got {} elements",
                    other.len()
                )));
            }
        },
        other => return Err(DmaError::Protocol(format!("unexpected reply {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::{Error, SendHello, negotiate, parse, request};
    use crate::dma::command::{self, TransferReply};
    use crate::dma::{Dialect, DmaError, TransferReceipt};
    use resp_proto::Value;

    fn receipt(reply: Value) -> Result<Option<TransferReceipt>, DmaError> {
        parse(reply)?.receipt()
    }

    /// Replies one by one; records the dialect of each hello sent.
    struct Script {
        replies: Vec<Result<Vec<Vec<u8>>, Error>>,
        sent: Vec<Dialect>,
    }

    impl SendHello for Script {
        async fn send_hello(&mut self, dialect: Dialect) -> Result<Vec<Vec<u8>>, Error> {
            self.sent.push(dialect);
            self.replies.remove(0)
        }
    }

    fn script(replies: Vec<Result<Vec<Vec<u8>>, Error>>) -> Script {
        Script {
            replies,
            sent: Vec::new(),
        }
    }

    fn refused(message: &str) -> Result<Vec<Vec<u8>>, Error> {
        Err(Error::Resp(ringline_redis::Error::Redis(message.into())))
    }

    #[tokio::test]
    async fn a_server_with_the_rdma_names_takes_one_hello() {
        let mut s = script(vec![Ok(vec![vec![1]])]);
        let (dialect, addresses) = negotiate(Dialect::LargeObj, &mut s).await.unwrap();
        assert_eq!(dialect, Dialect::LargeObjRdma);
        assert_eq!(addresses, [vec![1]]);
        assert_eq!(s.sent, [Dialect::LargeObjRdma]);
    }

    #[tokio::test]
    async fn a_server_without_them_gets_the_original_hello() {
        let mut s = script(vec![refused("ERR unknown command"), Ok(vec![vec![2]])]);
        let (dialect, _) = negotiate(Dialect::LargeObj, &mut s).await.unwrap();
        assert_eq!(dialect, Dialect::LargeObj);
        assert_eq!(s.sent, [Dialect::LargeObjRdma, Dialect::LargeObj]);
    }

    #[tokio::test]
    async fn two_refusals_report_both_replies() {
        let mut s = script(vec![refused("NOPERM rdma"), refused("ERR unknown command")]);
        let error = negotiate(Dialect::LargeObj, &mut s).await.unwrap_err();
        assert!(matches!(
            error,
            Error::Resp(ringline_redis::Error::Redis(_))
        ));
        let error = error.to_string();
        assert!(error.contains("NOPERM rdma"), "{error}");
        assert!(error.contains("ERR unknown command"), "{error}");
    }

    #[tokio::test]
    async fn a_closed_connection_is_not_retried() {
        let mut s = script(vec![Err(Error::Resp(
            ringline_redis::Error::ConnectionClosed,
        ))]);
        assert!(negotiate(Dialect::LargeObj, &mut s).await.is_err());
        assert_eq!(s.sent, [Dialect::LargeObjRdma]);
    }

    #[tokio::test]
    async fn a_closed_connection_on_the_second_hello_is_returned() {
        let mut s = script(vec![
            refused("ERR unknown command"),
            Err(Error::Resp(ringline_redis::Error::ConnectionClosed)),
        ]);
        let error = negotiate(Dialect::LargeObj, &mut s).await.unwrap_err();
        assert!(matches!(
            error,
            Error::Resp(ringline_redis::Error::ConnectionClosed)
        ));
        assert_eq!(s.sent, [Dialect::LargeObjRdma, Dialect::LargeObj]);
    }

    #[tokio::test]
    async fn vdma_sends_its_own_hello_once() {
        let mut s = script(vec![refused("ERR")]);
        assert!(negotiate(Dialect::Vdma, &mut s).await.is_err());
        assert_eq!(s.sent, [Dialect::Vdma]);
    }

    #[test]
    fn a_command_becomes_a_request_with_the_name_first() {
        let built = command::hello(Dialect::Vdma, &[]);
        let encoded_len = request(&built).encoded_len();
        // `*1\r\n$9\r\nDMA.HELLO\r\n`
        assert_eq!(encoded_len, 4 + 4 + 9 + 2);
    }

    #[test]
    fn a_null_reply_is_a_miss() {
        assert_eq!(receipt(Value::Null).unwrap(), None);
    }

    #[test]
    fn an_integer_reply_is_a_byte_count() {
        let parsed = receipt(Value::Integer(1024)).unwrap().unwrap();
        assert_eq!(parsed.bytes_written, 1024);
        assert_eq!(parsed.checksum, None);
    }

    #[test]
    fn a_pair_reply_carries_the_checksum() {
        let reply = Value::Array(vec![Value::Integer(64), Value::Integer(0xE306_9283)]);
        let parsed = receipt(reply).unwrap().unwrap();
        assert_eq!(parsed.bytes_written, 64);
        assert_eq!(parsed.checksum, Some(0xE306_9283));
    }

    /// `+OK` is how largeobj acknowledges a write.
    #[test]
    fn an_ok_status_is_an_acknowledgement() {
        let reply = Value::SimpleString(bytes::Bytes::from_static(b"OK"));
        assert_eq!(parse(reply).unwrap(), TransferReply::Acknowledged);

        let other = Value::SimpleString(bytes::Bytes::from_static(b"QUEUED"));
        assert!(parse(other).is_err());
    }

    /// A server error must not be read as a transfer, or a failed command looks like a short one.
    #[test]
    fn an_error_reply_is_an_error() {
        let reply = Value::Error(bytes::Bytes::from_static(b"ERR no such key"));
        assert!(receipt(reply).is_err());
    }

    #[test]
    fn a_malformed_pair_is_rejected() {
        let reply = Value::Array(vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
        ]);
        assert!(receipt(reply).is_err());
    }

    /// The range checks are `TransferReply`'s; this checks they are reached.
    #[test]
    fn a_negative_byte_count_is_rejected() {
        assert!(receipt(Value::Integer(-1)).is_err());
    }
}
