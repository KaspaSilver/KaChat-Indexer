use crate::operation::{
    SealedContextualMessageV1, SealedGroupControlV1, SealedGroupMessageV1, SealedHandshakeV2,
    SealedMessageOrSealedHandshakeVNone, SealedOperation, SealedPaymentV1, SealedSelfStashV1,
};
use tracing::warn;

/// Canonical KaChat protocol prefix. KaChat is its own network on Kaspa; all new chat content is
/// written with this prefix.
pub const PROTOCOL_PREFIX: &str = "kchat:";
/// Legacy Kasia prefix. Still read so pre-rebrand chat history keeps resolving; never written going
/// forward. (KaChat no longer interoperates with the Kasia network — this is read-only legacy.)
pub const LEGACY_PROTOCOL_PREFIX: &str = "ciph_msg:";
pub const VERSION_1_PART: &str = "1:";

pub fn parse_sealed_operation(payload_bytes: &[u8]) -> Option<SealedOperation<'_>> {
    // Dual-read: accept the canonical `kchat:` prefix and the legacy `ciph_msg:` prefix (same
    // payload grammar after the prefix), so old on-chain content still parses.
    let payload_without_protocol = payload_bytes
        .strip_prefix(PROTOCOL_PREFIX.as_bytes())
        .or_else(|| payload_bytes.strip_prefix(LEGACY_PROTOCOL_PREFIX.as_bytes()))?;
    if payload_without_protocol.is_empty() {
        return None;
    }

    match payload_without_protocol.strip_prefix(VERSION_1_PART.as_bytes()) {
        // Handle SealedHandshake or SealedMessage
        None => Some(SealedOperation::SealedMessageOrSealedHandshakeVNone(
            SealedMessageOrSealedHandshakeVNone {
                sealed_hex: payload_without_protocol,
            },
        )),
        Some([b'p', b'a', b'y', b':', sealed_hex @ ..]) => {
            Some(SealedOperation::PaymentV1(SealedPaymentV1 { sealed_hex }))
        }
        Some(
            [
                b'p',
                b'a',
                b'y',
                b'm',
                b'e',
                b'n',
                b't',
                b':',
                sealed_hex @ ..,
            ],
        ) => Some(SealedOperation::PaymentV1(SealedPaymentV1 { sealed_hex })),
        Some(
            [
                b'h',
                b'a',
                b'n',
                b'd',
                b's',
                b'h',
                b'a',
                b'k',
                b'e',
                b':',
                sealed_hex @ ..,
            ],
        ) => Some(SealedOperation::SealedHandshakeV2(SealedHandshakeV2 {
            sealed_hex,
        })),
        Some([b'c', b'o', b'm', b'm', b':', remaining @ ..]) => {
            let delimiter_idx = remaining.iter().position(|b| b == &b':')?;
            let alias = &remaining[..delimiter_idx];
            let contextual_message_hex = &remaining[delimiter_idx + 1..];
            Some(SealedOperation::ContextualMessageV1(
                SealedContextualMessageV1 {
                    alias,
                    sealed_hex: contextual_message_hex,
                },
            ))
        }
        Some(
            [
                b's',
                b'e',
                b'l',
                b'f',
                b'_',
                b's',
                b't',
                b'a',
                b's',
                b'h',
                b':',
                remaining @ ..,
            ],
        ) => {
            let delimiter_idx_option = remaining.iter().position(|b| b == &b':');

            match delimiter_idx_option {
                Some(delimiter_idx) => {
                    let key = &remaining[..delimiter_idx];
                    let sealed_data = &remaining[delimiter_idx + 1..];
                    Some(SealedOperation::SelfStashV1(SealedSelfStashV1 {
                        key: Some(key),
                        sealed_hex: sealed_data,
                    }))
                }
                None => Some(SealedOperation::SelfStashV1(SealedSelfStashV1 {
                    key: None,
                    sealed_hex: remaining,
                })),
            }
        }
        Some([b'g', b'c', b'o', b'm', b'm', b':', remaining @ ..]) => {
            let full = remaining;
            let fields: Vec<&[u8]> = remaining.split(|byte| *byte == b':').collect();
            let [
                blinded_group_id,
                epoch,
                sender_id,
                sender_pub,
                msg_id,
                ciphertext,
                signature,
            ] = fields.as_slice()
            else {
                return None;
            };
            if !is_fixed_hex(blinded_group_id, 32)
                || epoch.is_empty()
                || !epoch.iter().all(u8::is_ascii_digit)
                || std::str::from_utf8(epoch).ok()?.parse::<u64>().is_err()
                || !is_fixed_hex(sender_id, 32)
                || !is_fixed_hex(sender_pub, 32)
                || !is_fixed_hex(msg_id, 24)
                || !is_nonempty_hex(ciphertext)
                || !is_fixed_hex(signature, 64)
            {
                return None;
            }

            Some(SealedOperation::GroupMessageV1(SealedGroupMessageV1 {
                blinded_group_id,
                epoch,
                sender_id,
                sender_pub,
                msg_id,
                ciphertext,
                signature,
                sealed_hex: full,
            }))
        }
        Some([b'g', b'c', b't', b'l', b':', remaining @ ..]) => {
            let fields: Vec<&[u8]> = remaining.split(|byte| *byte == b':').collect();
            let (recipient_pubkey, encrypted_payload) = match fields.as_slice() {
                [encrypted_payload] if is_nonempty_hex(encrypted_payload) => {
                    (None, *encrypted_payload)
                }
                [recipient_pubkey, encrypted_payload]
                    if is_fixed_hex(recipient_pubkey, 32) && is_nonempty_hex(encrypted_payload) =>
                {
                    (Some(*recipient_pubkey), *encrypted_payload)
                }
                _ => return None,
            };
            Some(SealedOperation::GroupControlV1(SealedGroupControlV1 {
                recipient_pubkey,
                encrypted_payload,
            }))
        }
        Some(msg_type_and_content) => {
            let msg_type_and_content = faster_hex::hex_string(msg_type_and_content);
            warn!("Unknown operation type: {msg_type_and_content}");
            None
        }
    }
}

fn is_fixed_hex(value: &[u8], byte_len: usize) -> bool {
    value.len() == byte_len * 2 && value.iter().all(u8::is_ascii_hexdigit)
}

fn is_nonempty_hex(value: &[u8]) -> bool {
    !value.is_empty() && value.len().is_multiple_of(2) && value.iter().all(u8::is_ascii_hexdigit)
}
