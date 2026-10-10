//! The `DMA.*` (vdma) and `BLOB.*` (valkey-large-object) command vocabulary.

use crate::dma::advertisement::Advertisement;
use crate::dma::error::DmaError;

/// Which module's commands to speak.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Dialect {
    /// vdma's `DMA.*`: the client address is on every transfer; a transfer answers with a byte count.
    Vdma,
    /// valkey-large-object's `BLOB.*`: the client address is sent once in `BLOB.HELLO` and bound
    /// to the connection; a transfer names `<rkey> <addr> <len>` triples; `BLOB.SET` answers `OK`
    /// and `BLOB.GET` answers `[bytes, crc32c]`. The client sends no checksum.
    #[default]
    LargeObj,
}

/// Description of a transfer's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferReceipt {
    /// Byte length transferred.
    pub bytes_written: usize,
    /// CRC-32c, when requested.
    pub checksum: Option<u32>,
}

/// A transfer reply, destructured from a client's RESP value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferReply {
    /// Nil: the key was absent.
    Missing,
    /// A bare byte count.
    Bytes(i64),
    /// A `[bytes, checksum]` pair.
    BytesAndChecksum {
        /// Byte length transferred.
        bytes: i64,
        /// CRC-32c the server computed over those bytes.
        checksum: i64,
    },
    /// `OK`: how `BLOB.SET` reports a write, with no count.
    Acknowledged,
}

impl TransferReply {
    /// Range-check a read reply into a receipt. `None` for a missing key.
    pub fn receipt(self) -> Result<Option<TransferReceipt>, DmaError> {
        let (bytes_written, checksum) = match self {
            Self::Missing => return Ok(None),
            Self::Bytes(bytes_written) => (bytes_written, None),
            Self::BytesAndChecksum { bytes, checksum } => {
                let checksum = u32::try_from(checksum)
                    .map_err(|_| DmaError::Protocol(format!("checksum out of range {checksum}")))?;
                (bytes, Some(checksum))
            }
            Self::Acknowledged => {
                return Err(DmaError::Protocol(
                    "OK where a byte count was expected".into(),
                ));
            }
        };
        let bytes_written = usize::try_from(bytes_written)
            .map_err(|_| DmaError::Protocol(format!("negative byte count {bytes_written}")))?;
        Ok(Some(TransferReceipt {
            bytes_written,
            checksum,
        }))
    }

    /// A write reply into a receipt for `length` offered bytes: vdma's sent byte count must be equal,
    /// largeobj says `OK`.
    pub fn write_receipt(self, length: usize) -> Result<TransferReceipt, DmaError> {
        if let Self::Acknowledged = self {
            return Ok(TransferReceipt {
                bytes_written: length,
                checksum: None,
            });
        }
        let receipt = self
            .receipt()?
            .ok_or_else(|| DmaError::Protocol("a write reported no transfer".into()))?;
        if receipt.bytes_written != length {
            return Err(DmaError::Integrity(format!(
                "server took {} of {length} bytes",
                receipt.bytes_written
            )));
        }
        Ok(receipt)
    }
}

/// Options for `DMA.SET`. vdma only; [`set`] refuses them under [`Dialect::LargeObj`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DmaSetOptions {
    checksum: Option<u32>,
}

impl DmaSetOptions {
    /// Send `checksum` for the server to verify against the bytes it receives.
    /// A mismatch aborts the set.
    pub fn with_checksum(mut self, checksum: u32) -> Self {
        self.checksum = Some(checksum);
        self
    }
}

/// Options for `DMA.GET`. vdma only, as [`DmaSetOptions`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DmaGetOptions {
    checksum: bool,
}

impl DmaGetOptions {
    /// Ask the server to return a CRC-32c of the value.
    pub fn with_checksum(mut self) -> Self {
        self.checksum = true;
        self
    }
}

/// A `DMA.*` or `BLOB.*` command: its name, and its arguments in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmaCommand {
    name: &'static str,
    arguments: Vec<Vec<u8>>,
}

impl DmaCommand {
    /// The command name, as sent.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The arguments, in the order they go on the wire.
    pub fn arguments(&self) -> &[Vec<u8>] {
        &self.arguments
    }
}

/// `DMA.HELLO`, or `BLOB.HELLO <client-address>`. Only largeobj sends `local_address`.
pub fn hello(dialect: Dialect, local_address: &[u8]) -> DmaCommand {
    match dialect {
        Dialect::Vdma => DmaCommand {
            name: "DMA.HELLO",
            arguments: Vec::new(),
        },
        Dialect::LargeObj => DmaCommand {
            name: "BLOB.HELLO",
            arguments: vec![crate::dma::advertisement::encode_hex(local_address).into_bytes()],
        },
    }
}

/// `DMA.INFO` — dump diagnostics about the module's environment.
pub fn info() -> DmaCommand {
    DmaCommand {
        name: "DMA.INFO",
        arguments: Vec::new(),
    }
}

/// `DMA.SET <address> <rkey> <remote-address> <length> <key> [<crc>]`, or
/// `BLOB.SET <key> <length> <rkey> <remote-address> <length>`.
pub fn set(
    dialect: Dialect,
    advertisement: &Advertisement,
    key: &[u8],
    length: usize,
    options: &DmaSetOptions,
) -> Result<DmaCommand, DmaError> {
    match dialect {
        Dialect::Vdma => {
            let mut command = transfer("DMA.SET", advertisement, key, length);
            if let Some(checksum) = options.checksum {
                command.arguments.push(number(u64::from(checksum)));
            }
            Ok(command)
        }
        Dialect::LargeObj => {
            if options.checksum.is_some() {
                return Err(DmaError::Configuration(
                    "largeobj takes no checksum on a write".into(),
                ));
            }
            let mut arguments = vec![key.to_vec(), number(length as u64)];
            arguments.extend(address_triple(advertisement, length));
            Ok(DmaCommand {
                name: "BLOB.SET",
                arguments,
            })
        }
    }
}

/// `DMA.GET <address> <rkey> <remote-address> <capacity> <key> [<crc-flag>]`, or
/// `BLOB.GET <key> <rkey> <remote-address> <capacity>`. largeobj returns a CRC-32c unconditionally.
pub fn get(
    dialect: Dialect,
    advertisement: &Advertisement,
    key: &[u8],
    capacity: usize,
    options: &DmaGetOptions,
) -> Result<DmaCommand, DmaError> {
    match dialect {
        Dialect::Vdma => {
            let mut command = transfer("DMA.GET", advertisement, key, capacity);
            if options.checksum {
                // The flag's presence is what asks for a checksum; the value is unread.
                command.arguments.push(number(0));
            }
            Ok(command)
        }
        Dialect::LargeObj => {
            let mut arguments = vec![key.to_vec()];
            arguments.extend(address_triple(advertisement, capacity));
            Ok(DmaCommand {
                name: "BLOB.GET",
                arguments,
            })
        }
    }
}

fn transfer(
    name: &'static str,
    advertisement: &Advertisement,
    key: &[u8],
    length: usize,
) -> DmaCommand {
    let mut arguments: Vec<Vec<u8>> = advertisement
        .to_args()
        .into_iter()
        .map(String::into_bytes)
        .collect();
    arguments.push(number(length as u64));
    arguments.push(key.to_vec());
    DmaCommand { name, arguments }
}

/// `<rkey> <remote-address> <length>`: one largeobj address, the advertisement without its
/// fabric address.
fn address_triple(advertisement: &Advertisement, length: usize) -> impl Iterator<Item = Vec<u8>> {
    advertisement
        .to_region_args()
        .into_iter()
        .map(String::into_bytes)
        .chain(std::iter::once(number(length as u64)))
}

/// Decimal ASCII, matching how a RESP client renders an integer argument.
fn number(value: u64) -> Vec<u8> {
    value.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{Dialect, DmaGetOptions, DmaSetOptions, TransferReply, get, hello, info, set};
    use crate::dma::advertisement::Advertisement;

    fn advertisement() -> Advertisement {
        Advertisement {
            address: vec![0xde, 0xad],
            remote_key: 7,
            remote_address: 0x1000,
        }
    }

    fn arguments(command: &super::DmaCommand) -> Vec<String> {
        command
            .arguments()
            .iter()
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect()
    }

    #[test]
    fn set_lays_out_arguments_in_wire_order() {
        let command = set(
            Dialect::Vdma,
            &advertisement(),
            b"key",
            64,
            &DmaSetOptions::default(),
        )
        .unwrap();
        assert_eq!(command.name(), "DMA.SET");
        assert_eq!(arguments(&command), ["dead", "7", "4096", "64", "key"]);
    }

    #[test]
    fn get_lays_out_arguments_in_wire_order() {
        let command = get(
            Dialect::Vdma,
            &advertisement(),
            b"key",
            64,
            &DmaGetOptions::default(),
        )
        .unwrap();
        assert_eq!(command.name(), "DMA.GET");
        assert_eq!(arguments(&command), ["dead", "7", "4096", "64", "key"]);
    }

    #[test]
    fn set_appends_the_checksum_last() {
        let options = DmaSetOptions::default().with_checksum(0xabcd);
        let command = set(Dialect::Vdma, &advertisement(), b"key", 64, &options).unwrap();
        assert_eq!(arguments(&command).last().unwrap(), "43981");
    }

    #[test]
    fn get_appends_a_flag_only_when_a_checksum_is_wanted() {
        let plain = get(
            Dialect::Vdma,
            &advertisement(),
            b"key",
            64,
            &DmaGetOptions::default(),
        )
        .unwrap();
        assert_eq!(plain.arguments().len(), 5);

        let wanted = get(
            Dialect::Vdma,
            &advertisement(),
            b"key",
            64,
            &DmaGetOptions::default().with_checksum(),
        )
        .unwrap();
        assert_eq!(wanted.arguments().len(), 6);
    }

    /// A key is bytes, not text, and must survive unaltered.
    #[test]
    fn a_key_is_carried_as_raw_bytes() {
        let command = set(
            Dialect::Vdma,
            &advertisement(),
            &[0x00, 0xff],
            1,
            &DmaSetOptions::default(),
        )
        .unwrap();
        assert_eq!(command.arguments().last().unwrap().as_slice(), [0x00, 0xff]);
    }

    #[test]
    fn a_largeobj_hello_carries_the_client_address_as_hex() {
        let command = hello(Dialect::LargeObj, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(command.name(), "BLOB.HELLO");
        assert_eq!(arguments(&command), ["deadbeef"]);

        // vdma learns the address from each transfer instead.
        let command = hello(Dialect::Vdma, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(command.name(), "DMA.HELLO");
        assert!(command.arguments().is_empty());
    }

    /// The key leads and the address is absent: the server already holds it from the hello.
    #[test]
    fn a_largeobj_set_lays_out_arguments_in_wire_order() {
        let command = set(
            Dialect::LargeObj,
            &advertisement(),
            b"key",
            64,
            &DmaSetOptions::default(),
        )
        .unwrap();
        assert_eq!(command.name(), "BLOB.SET");
        assert_eq!(arguments(&command), ["key", "64", "7", "4096", "64"]);
    }

    /// The capacity is the triple's length: the server refuses an object it cannot hold.
    #[test]
    fn a_largeobj_get_lays_out_arguments_in_wire_order() {
        let command = get(
            Dialect::LargeObj,
            &advertisement(),
            b"key",
            64,
            &DmaGetOptions::default(),
        )
        .unwrap();
        assert_eq!(command.name(), "BLOB.GET");
        assert_eq!(arguments(&command), ["key", "7", "4096", "64"]);
    }

    #[test]
    fn largeobj_refuses_a_write_checksum_rather_than_dropping_it() {
        let set_options = DmaSetOptions::default().with_checksum(0xabcd);
        assert!(
            set(
                Dialect::LargeObj,
                &advertisement(),
                b"key",
                64,
                &set_options
            )
            .is_err()
        );
    }

    /// The server returns a CRC either way, so asking changes nothing on the wire.
    #[test]
    fn a_largeobj_get_asks_for_no_checksum() {
        let get_options = DmaGetOptions::default().with_checksum();
        let command = get(
            Dialect::LargeObj,
            &advertisement(),
            b"key",
            64,
            &get_options,
        )
        .unwrap();
        assert_eq!(arguments(&command), ["key", "7", "4096", "64"]);
    }

    #[test]
    fn an_ok_is_a_write_of_everything_offered() {
        let receipt = TransferReply::Acknowledged.write_receipt(64).unwrap();
        assert_eq!(receipt.bytes_written, 64);
        assert_eq!(receipt.checksum, None);
    }

    #[test]
    fn a_short_write_count_is_an_integrity_error() {
        assert!(TransferReply::Bytes(64).write_receipt(64).is_ok());
        assert!(TransferReply::Bytes(63).write_receipt(64).is_err());
        assert!(TransferReply::Missing.write_receipt(64).is_err());
    }

    /// A read reports a count; an `OK` there means the wrong command was sent.
    #[test]
    fn an_ok_is_not_a_read_receipt() {
        assert!(TransferReply::Acknowledged.receipt().is_err());
    }

    #[test]
    fn a_missing_key_has_no_receipt() {
        assert_eq!(super::TransferReply::Missing.receipt().unwrap(), None);
    }

    #[test]
    fn a_byte_count_becomes_a_receipt() {
        let receipt = super::TransferReply::Bytes(1024)
            .receipt()
            .unwrap()
            .unwrap();
        assert_eq!(receipt.bytes_written, 1024);
        assert_eq!(receipt.checksum, None);
    }

    #[test]
    fn a_checksum_rides_along_with_the_byte_count() {
        let reply = super::TransferReply::BytesAndChecksum {
            bytes: 1024,
            checksum: 0xE306_9283,
        };
        let receipt = reply.receipt().unwrap().unwrap();
        assert_eq!(receipt.bytes_written, 1024);
        assert_eq!(receipt.checksum, Some(0xE306_9283));
    }

    /// A negative count is a corrupt reply.
    #[test]
    fn a_negative_byte_count_is_rejected() {
        assert!(super::TransferReply::Bytes(-1).receipt().is_err());
    }

    #[test]
    fn a_checksum_outside_u32_is_rejected() {
        let reply = super::TransferReply::BytesAndChecksum {
            bytes: 1,
            checksum: i64::from(u32::MAX) + 1,
        };
        assert!(reply.receipt().is_err());
    }

    #[test]
    fn handshake_commands_carry_no_arguments() {
        assert_eq!(hello(Dialect::Vdma, &[]).name(), "DMA.HELLO");
        assert!(hello(Dialect::Vdma, &[]).arguments().is_empty());
        assert_eq!(info().name(), "DMA.INFO");
        assert!(info().arguments().is_empty());
    }
}
