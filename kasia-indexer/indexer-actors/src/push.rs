use indexer_db::AddressPayload;

#[derive(Debug, Clone, Copy)]
pub enum PushEventKind {
    Contextual,
    Payment,
    Handshake,
    SelfStash,
    GroupMessage,
    GroupControl,
}

/// KaChat fork: broadcast + KaPosts pushes. These are injected by the K-processor over an
/// internal HTTP endpoint (not derived from the chat block stream), already carrying display-ready
/// text, so they ride a separate channel into the same dispatcher/APNs path.
#[derive(Debug, Clone)]
pub enum ExtensionPushEvent {
    Broadcast {
        channel: String,
        /// Sender's kaspa address — used to skip the sender's own device(s) and hidden-sender filters.
        sender_address: String,
        subtitle: String,
        body: String,
        /// Hex tx id — APNs collapse-id (retry dedupe).
        tx_id: String,
    },
    KaPosts {
        /// Registered `kaposts_pubkey` whose content was acted on.
        target_pubkey: String,
        /// Actor's pubkey — devices registered as the actor are skipped (no self-pings).
        actor_pubkey: String,
        /// Action kind (like/dislike/comment/repost/follow) for per-type toggle filtering; `None`
        /// = always notify.
        action: Option<String>,
        subtitle: String,
        body: String,
        /// Target content txid (present when the action targets content; omitted for follows).
        post_id: Option<String>,
        /// Action txid — APNs collapse-id.
        tx_id: String,
    },
}

/// Address Activity: an accepted tx credited one or more watched (owned / watch-only) addresses
/// with KAS. Fed from the block processor to the push dispatcher, which matches each credited
/// address to the devices watching it, sums per device, self-send-filters (skips when `sender` is
/// one of that device's own addresses), rate-limits, and sends an `address_activity` push.
#[derive(Debug, Clone)]
pub struct FundsPushEvent {
    /// Watched outputs credited by this tx: (address, amount in sompi).
    pub credited: Vec<(AddressPayload, u64)>,
    /// Resolved input (sender) address, if known — used for self-send filtering.
    pub sender: Option<AddressPayload>,
    /// Accepting tx id.
    pub tx_id: [u8; 32],
    pub timestamp: u64,
    pub daa_score: u64,
}

#[derive(Debug, Clone)]
pub struct PushEvent {
    pub kind: PushEventKind,
    pub watched_address: AddressPayload,
    pub sender: AddressPayload,
    pub receiver: AddressPayload,
    pub alias: Option<String>,
    pub tx_id: [u8; 32],
    pub amount: Option<u64>,
    pub payload: Option<String>,
    pub timestamp: u64,
    pub daa_score: u64,
    pub blinded_group_id: Option<[u8; 32]>,
    /// Exact destination for recipient-addressed `gctl`; `None` denotes legacy control.
    pub group_control_recipient: Option<AddressPayload>,
}

pub fn parse_self_stash_alias(raw: &[u8]) -> Option<String> {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    if end == 0 {
        return None;
    }
    let alias = std::str::from_utf8(&raw[..end]).ok()?.trim();
    if alias.is_empty() {
        return None;
    }
    Some(alias.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_self_stash_alias;

    #[test]
    fn parses_plain_alias() {
        assert_eq!(
            parse_self_stash_alias(b"alias123"),
            Some("alias123".to_string())
        );
    }

    #[test]
    fn parses_zero_padded_alias() {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(b"alias123");
        assert_eq!(parse_self_stash_alias(&bytes), Some("alias123".to_string()));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            parse_self_stash_alias(b"  alias123  "),
            Some("alias123".to_string())
        );
    }

    #[test]
    fn rejects_empty_alias() {
        assert_eq!(parse_self_stash_alias(b""), None);
        assert_eq!(parse_self_stash_alias(b"   "), None);
        assert_eq!(parse_self_stash_alias(&[0u8; 8]), None);
    }

    #[test]
    fn rejects_invalid_utf8() {
        assert_eq!(parse_self_stash_alias(&[0xFF, 0xFE]), None);
    }
}
