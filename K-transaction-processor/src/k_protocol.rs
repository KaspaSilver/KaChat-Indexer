use crate::database::{DbPool, Transaction};
use crate::hashtag_extractor::extract_hashtags_from_base64;
use anyhow::Result;
use hex;
use serde_json;
use tracing::{error, info, warn};

// Kaspa message signature verification imports (from main K-indexer)
use kaspa_wallet_core::message::{PersonalMessage, verify_message};
use secp256k1::XOnlyPublicKey;

// K Protocol Data Models (ported from main K-indexer)
use serde::{Deserialize, Serialize};

// ============================================================================
// KaChat exclusivity (fork addition)
// ============================================================================

/// KaChat exclusivity marker: U+2060 WORD JOINER (UTF-8 bytes E2 81 A0), prepended inside
/// every KaChat message before base64 encoding. This fork enforces two-way exclusivity
/// server-side: only content whose decoded message begins with this marker is indexed, so
/// K-website content never enters the KaPosts database. Because the marker is always the
/// first three content bytes, its standard-base64 encoding is always the leading group
/// "4oGg" (E2 81 A0 -> 4oGg), which the client relies on as well.
pub const KACHAT_MARKER: &str = "\u{2060}";

/// Channels the broadcast indexer tracks. Only these normalized names are stored; everything
/// else on the `ciph_msg:1:bcast:` protocol is dropped.
pub const BROADCAST_CHANNELS: [&str; 2] = ["kaspa", "kachat-bugs"];

/// Max characters of broadcast content stored (safety cap; broadcasts may carry voice/reply
/// JSON envelopes, so this is generous — the on-chain payload size is the real limit).
pub const MAX_BROADCAST_CONTENT_CHARS: usize = 65_536;

/// Max characters allowed in a KaChat message body (after the marker). Generous for text but
/// far below what an embedded media blob needs — a second line of defence against base64
/// payloads slipping in disguised as "text".
pub const MAX_KACHAT_MESSAGE_CHARS: usize = 4096;

/// Validate that a base64-encoded message is acceptable KaChat content and gate every content
/// insert (post / reply / quote / repost) on it. KaPosts is text-only; this enforces that
/// server-side regardless of which client produced the transaction. A message is accepted
/// only if it is:
///   - valid base64 of valid UTF-8,
///   - prefixed with the KaChat exclusivity marker (U+2060),
///   - within `MAX_KACHAT_MESSAGE_CHARS`,
///   - free of embedded media / data URIs (`data:image/…`, `;base64,`, etc.),
///   - free of control characters other than tab / newline / carriage-return.
/// Returns `Err(reason)` describing why a message was rejected. (A plain repost — body that is
/// exactly the marker — has an empty body and passes.)
pub fn validate_kachat_message(base64_encoded_message: &str) -> Result<(), &'static str> {
    use base64::{Engine as _, engine::general_purpose};
    let bytes = general_purpose::STANDARD
        .decode(base64_encoded_message)
        .map_err(|_| "invalid base64")?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "not valid UTF-8")?;
    let body = text.strip_prefix(KACHAT_MARKER).ok_or("missing KaChat marker")?;

    if body.chars().count() > MAX_KACHAT_MESSAGE_CHARS {
        return Err("message exceeds max length");
    }

    // Embedded-media / data-URI / base64-blob signatures (ASCII, case-insensitive).
    let lower = body.to_ascii_lowercase();
    if lower.contains(";base64,")
        || lower.contains("data:image/")
        || lower.contains("data:video/")
        || lower.contains("data:audio/")
        || lower.contains("data:application/")
        || lower.contains("data:text/html")
    {
        return Err("embedded media / data URI not allowed");
    }

    // Reject control characters except the common text whitespace.
    if body
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
    {
        return Err("contains control characters");
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum KActionType {
    Broadcast(KBroadcast),
    Post(KPost),
    Reply(KReply),
    Vote(KVote),
    Block(KBlock),
    Quote(KQuote),
    Follow(KFollow),
    /// Removal counter-action: withdraws the sender's prior quote/repost of a content id
    /// (fork addition; mirrors follow/unfollow). Payload: unquote:pubkey:sig:content_id
    Unquote(KUnquote),
    Unknown(String),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KBroadcast {
    pub sender_pubkey: String,
    pub sender_signature: String,
    #[serde(default)]
    pub base64_encoded_nickname: String,
    pub base64_encoded_profile_image: Option<String>,
    pub base64_encoded_message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KPost {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub base64_encoded_message: String,
    pub mentioned_pubkeys: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KReply {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub post_id: String,
    pub base64_encoded_message: String,
    pub mentioned_pubkeys: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KVote {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub post_id: String,
    pub vote: String, // "upvote" or "downvote"
    pub mentioned_pubkey: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KBlock {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub blocking_action: String, // "block" or "unblock"
    pub blocked_user_pubkey: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KQuote {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub content_id: String,
    pub base64_encoded_message: String,
    pub mentioned_pubkey: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KFollow {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub following_action: String, // "follow" or "unfollow"
    pub followed_user_pubkey: String,
}

/// Removal counter-action for a quote/repost (fork addition). The sender withdraws their
/// prior quote or repost of `content_id`; on receipt the processor deletes the matching
/// k_contents row(s) and quotesCount (computed live) drops automatically.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KUnquote {
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub content_id: String,
}

// Database record structures for PostgreSQL
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KPostRecord {
    pub transaction_id: String,
    pub block_time: i64,
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub base64_encoded_message: String,
    pub mentioned_pubkeys: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KReplyRecord {
    pub transaction_id: String,
    pub block_time: i64,
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub post_id: String,
    pub base64_encoded_message: String,
    pub mentioned_pubkeys: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KBroadcastRecord {
    pub transaction_id: String,
    pub block_time: i64,
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub base64_encoded_nickname: String,
    pub base64_encoded_profile_image: Option<String>,
    pub base64_encoded_message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KVoteRecord {
    pub transaction_id: String,
    pub block_time: i64,
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub post_id: String,
    pub vote: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KBlockRecord {
    pub transaction_id: String,
    pub block_time: i64,
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub blocking_action: String,
    pub blocked_user_pubkey: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KFollowRecord {
    pub transaction_id: String,
    pub block_time: i64,
    pub sender_pubkey: String,
    pub sender_signature: String,
    pub following_action: String,
    pub followed_user_pubkey: String,
}

pub struct KProtocolProcessor {
    db_pool: DbPool,
    /// Network name ("mainnet" / "testnet-10"), used to prefix broadcast sender addresses.
    network: String,
}

impl KProtocolProcessor {
    pub fn new(db_pool: DbPool, network: String) -> Self {
        Self { db_pool, network }
    }

    /// Bech32 human-readable prefix for the active network.
    fn address_hrp(&self) -> &'static str {
        if self.network == "mainnet" {
            "kaspa"
        } else {
            "kaspatest"
        }
    }

    /// Verify a Kaspa message signature using the proper kaspa-wallet-core verification
    /// This uses Kaspa's PersonalMessageSigningHash and Schnorr signature verification
    fn verify_kaspa_signature(&self, message: &str, signature: &str, public_key_hex: &str) -> bool {
        // Create PersonalMessage from the message string
        let personal_message = PersonalMessage(message);

        // Parse signature from hex (64 bytes for Schnorr signature)
        let signature_bytes = match hex::decode(signature) {
            Ok(bytes) => {
                if bytes.len() != 64 {
                    error!(
                        "Invalid signature length: expected 64 bytes, got {}",
                        bytes.len()
                    );
                    return false;
                }
                bytes
            }
            Err(err) => {
                error!("Failed to decode signature hex '{}': {}", signature, err);
                return false;
            }
        };

        // Parse public key from hex
        let public_key_bytes = match hex::decode(public_key_hex) {
            Ok(bytes) => {
                if bytes.len() == 33 {
                    // Remove the compression prefix byte for x-only key (Schnorr uses x-only keys)
                    bytes[1..].to_vec()
                } else if bytes.len() == 32 {
                    // Already x-only format
                    bytes
                } else {
                    error!(
                        "Invalid public key length: expected 32 or 33 bytes, got {}",
                        bytes.len()
                    );
                    return false;
                }
            }
            Err(err) => {
                error!(
                    "Failed to decode public key hex '{}': {}",
                    public_key_hex, err
                );
                return false;
            }
        };

        // Create XOnlyPublicKey for verification
        let public_key = match XOnlyPublicKey::from_slice(&public_key_bytes) {
            Ok(key) => key,
            Err(err) => {
                error!("Failed to create XOnlyPublicKey: {}", err);
                return false;
            }
        };

        // Verify the message signature using Kaspa's verify_message function
        match verify_message(&personal_message, &signature_bytes, &public_key) {
            Ok(()) => {
                //info!("Kaspa message signature verification successful");
                true
            }
            Err(err) => {
                error!("Kaspa message signature verification failed: {}", err);
                false
            }
        }
    }

    /// Parse K protocol payload and extract action type
    pub fn parse_k_protocol_payload(&self, payload: &str) -> Result<KActionType> {
        // Remove the K protocol prefix "k:1:"
        if !payload.starts_with("k:1:") {
            return Err(anyhow::anyhow!("Invalid K protocol prefix"));
        }

        let k_payload = &payload[4..]; // Remove "k:1:" prefix

        // Split by colons to get the components
        let parts: Vec<&str> = k_payload.split(':').collect();

        if parts.is_empty() {
            return Err(anyhow::anyhow!(
                "Empty K protocol payload after removing prefix"
            ));
        }

        let action = parts[0];

        match action {
            "broadcast" => {
                // Expected format: broadcast:sender_pubkey:sender_signature:base64_encoded_nickname:base64_encoded_profile_image:base64_encoded_message
                if parts.len() < 6 {
                    return Err(anyhow::anyhow!(
                        "Invalid broadcast format: expected 6 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let base64_encoded_nickname = parts[3].to_string();
                let base64_encoded_profile_image = if parts[4].is_empty() {
                    None
                } else {
                    Some(parts[4].to_string())
                };
                let base64_encoded_message = parts[5].to_string();

                Ok(KActionType::Broadcast(KBroadcast {
                    sender_pubkey,
                    sender_signature,
                    base64_encoded_nickname,
                    base64_encoded_profile_image,
                    base64_encoded_message,
                }))
            }
            "post" => {
                // Expected format: post:sender_pubkey:sender_signature:base64_message:mentioned_pubkeys_json
                if parts.len() < 4 {
                    return Err(anyhow::anyhow!(
                        "Invalid post format: expected at least 4 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let base64_encoded_message = parts[3].to_string();

                // Parse mentioned_pubkeys from JSON if present
                let mentioned_pubkeys: Vec<String> = if parts.len() > 4 {
                    let mentioned_pubkeys_json = parts[4];
                    match serde_json::from_str::<Vec<String>>(mentioned_pubkeys_json) {
                        Ok(pubkeys) => pubkeys,
                        Err(err) => {
                            error!(
                                "Failed to parse mentioned_pubkeys JSON '{}': {}",
                                mentioned_pubkeys_json, err
                            );
                            Vec::new() // Default to empty array on parse error
                        }
                    }
                } else {
                    Vec::new() // No mentioned_pubkeys field
                };

                Ok(KActionType::Post(KPost {
                    sender_pubkey,
                    sender_signature,
                    base64_encoded_message,
                    mentioned_pubkeys,
                }))
            }
            "reply" => {
                // Expected format: reply:sender_pubkey:sender_signature:post_id:base64_message:mentioned_pubkeys_json
                if parts.len() < 5 {
                    return Err(anyhow::anyhow!(
                        "Invalid reply format: expected at least 5 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let post_id = parts[3].to_string();
                let base64_encoded_message = parts[4].to_string();

                // Parse mentioned_pubkeys from JSON if present
                let mentioned_pubkeys: Vec<String> = if parts.len() > 5 {
                    let mentioned_pubkeys_json = parts[5];
                    match serde_json::from_str::<Vec<String>>(mentioned_pubkeys_json) {
                        Ok(pubkeys) => pubkeys,
                        Err(err) => {
                            error!(
                                "Failed to parse mentioned_pubkeys JSON '{}': {}",
                                mentioned_pubkeys_json, err
                            );
                            Vec::new() // Default to empty array on parse error
                        }
                    }
                } else {
                    Vec::new() // No mentioned_pubkeys field
                };

                Ok(KActionType::Reply(KReply {
                    sender_pubkey,
                    sender_signature,
                    post_id,
                    base64_encoded_message,
                    mentioned_pubkeys,
                }))
            }
            "vote" => {
                // Expected format: vote:sender_pubkey:sender_signature:post_id:vote:mentioned_pubkey
                if parts.len() < 6 {
                    return Err(anyhow::anyhow!(
                        "Invalid vote format: expected 6 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let post_id = parts[3].to_string();
                let vote = parts[4].to_string();
                let mentioned_pubkey = parts[5].to_string();

                // Validate vote value. 'unvote' (fork addition) is a removal counter-action:
                // it withdraws the sender's prior upvote/downvote on this post.
                if vote != "upvote" && vote != "downvote" && vote != "unvote" {
                    return Err(anyhow::anyhow!(
                        "Invalid vote value: expected 'upvote', 'downvote' or 'unvote', got '{}'",
                        vote
                    ));
                }

                Ok(KActionType::Vote(KVote {
                    sender_pubkey,
                    sender_signature,
                    post_id,
                    vote,
                    mentioned_pubkey,
                }))
            }
            "block" => {
                // Expected format: block:sender_pubkey:sender_signature:blocking_action:blocked_user_pubkey
                if parts.len() < 5 {
                    return Err(anyhow::anyhow!(
                        "Invalid block format: expected 5 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let blocking_action = parts[3].to_string();
                let blocked_user_pubkey = parts[4].to_string();

                // Validate blocking_action value
                if blocking_action != "block" && blocking_action != "unblock" {
                    return Err(anyhow::anyhow!(
                        "Invalid blocking_action value: expected 'block' or 'unblock', got '{}'",
                        blocking_action
                    ));
                }

                Ok(KActionType::Block(KBlock {
                    sender_pubkey,
                    sender_signature,
                    blocking_action,
                    blocked_user_pubkey,
                }))
            }
            "quote" => {
                // Expected format: quote:sender_pubkey:sender_signature:content_id:base64_encoded_message:mentioned_pubkey
                if parts.len() < 6 {
                    return Err(anyhow::anyhow!(
                        "Invalid quote format: expected 6 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let content_id = parts[3].to_string();
                let base64_encoded_message = parts[4].to_string();
                let mentioned_pubkey = parts[5].to_string();

                Ok(KActionType::Quote(KQuote {
                    sender_pubkey,
                    sender_signature,
                    content_id,
                    base64_encoded_message,
                    mentioned_pubkey,
                }))
            }
            "follow" => {
                // Expected format: follow:sender_pubkey:sender_signature:following_action:followed_user_pubkey
                if parts.len() < 5 {
                    return Err(anyhow::anyhow!(
                        "Invalid follow format: expected 5 parts, got {}",
                        parts.len()
                    ));
                }

                let sender_pubkey = parts[1].to_string();
                let sender_signature = parts[2].to_string();
                let following_action = parts[3].to_string();
                let followed_user_pubkey = parts[4].to_string();

                // Validate following_action value
                if following_action != "follow" && following_action != "unfollow" {
                    return Err(anyhow::anyhow!(
                        "Invalid following_action value: expected 'follow' or 'unfollow', got '{}'",
                        following_action
                    ));
                }

                Ok(KActionType::Follow(KFollow {
                    sender_pubkey,
                    sender_signature,
                    following_action,
                    followed_user_pubkey,
                }))
            }
            "unquote" => {
                // Expected format: unquote:sender_pubkey:sender_signature:content_id
                // (fork addition) Removal counter-action for a prior quote/repost.
                if parts.len() < 4 {
                    return Err(anyhow::anyhow!(
                        "Invalid unquote format: expected 4 parts, got {}",
                        parts.len()
                    ));
                }

                Ok(KActionType::Unquote(KUnquote {
                    sender_pubkey: parts[1].to_string(),
                    sender_signature: parts[2].to_string(),
                    content_id: parts[3].to_string(),
                }))
            }
            _ => Ok(KActionType::Unknown(action.to_string())),
        }
    }

    /// Process K protocol transaction
    pub async fn process_k_transaction(&self, transaction: &Transaction) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Get payload as hex string
        let payload_hex = match &transaction.payload {
            Some(hex_payload) => hex_payload,
            None => {
                warn!("Transaction {} has no payload", transaction_id);
                return Ok(());
            }
        };

        // Convert hex payload to UTF-8 string
        let payload_bytes = match hex::decode(payload_hex) {
            Ok(bytes) => bytes,
            Err(err) => {
                error!(
                    "Failed to decode hex payload for transaction {}: {}",
                    transaction_id, err
                );
                return Ok(());
            }
        };

        let payload_str = match std::str::from_utf8(&payload_bytes) {
            Ok(payload_str) => payload_str,
            Err(err) => {
                error!(
                    "Invalid UTF-8 in transaction payload for ID: {}: {}",
                    transaction_id, err
                );
                return Ok(());
            }
        };

        // KaChat broadcast (fork addition): `ciph_msg:1:bcast:<channel>:<content>`. Different
        // protocol family from `k:1:` — route it before K parsing, and store the content
        // verbatim from the RAW payload (not the control-char-cleaned one).
        if let Some(rest) = payload_str.strip_prefix("ciph_msg:1:bcast:") {
            return self.process_broadcast(transaction, rest).await;
        }

        // Clean the payload string by removing null bytes and other control characters
        let cleaned_payload = payload_str
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t')
            .collect::<String>();

        // Parse K protocol payload
        match self.parse_k_protocol_payload(&cleaned_payload) {
            Ok(action_type) => match action_type {
                KActionType::Broadcast(k_broadcast) => {
                    self.save_k_broadcast_to_database(transaction, k_broadcast)
                        .await?;
                }
                KActionType::Post(k_post) => {
                    self.save_k_post_to_database(transaction, k_post).await?;
                }
                KActionType::Reply(k_reply) => {
                    self.save_k_reply_to_database(transaction, k_reply).await?;
                }
                KActionType::Vote(k_vote) => {
                    self.save_k_vote_to_database(transaction, k_vote).await?;
                }
                KActionType::Block(k_block) => {
                    self.process_k_block_in_database(transaction, k_block)
                        .await?;
                }
                KActionType::Quote(k_quote) => {
                    self.save_k_quote_to_database(transaction, k_quote).await?;
                }
                KActionType::Follow(k_follow) => {
                    self.process_k_follow_in_database(transaction, k_follow)
                        .await?;
                }
                KActionType::Unquote(k_unquote) => {
                    self.process_k_unquote_in_database(transaction, k_unquote)
                        .await?;
                }
                KActionType::Unknown(action) => {
                    warn!(
                        "Unknown K protocol action '{}' in transaction {}",
                        action, transaction_id
                    );
                }
            },
            Err(err) => {
                error!(
                    "Failed to parse K protocol payload for transaction {}: {}",
                    transaction_id, err
                );
            }
        }

        Ok(())
    }

    /// Store a KaChat broadcast (fork addition). `rest` is the payload after the
    /// `ciph_msg:1:bcast:` prefix, i.e. `<channel>:<content>`. Content is stored verbatim
    /// (may be plain text or a reply/audio JSON envelope). Deduped by transaction id.
    async fn process_broadcast(&self, transaction: &Transaction, rest: &str) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // channel = up to the first ':'; content = the remainder verbatim (may contain ':').
        let (channel_raw, content) = match rest.split_once(':') {
            Some((c, body)) => (c, body),
            None => {
                info!(
                    "Broadcast {} has no channel/content separator, skipping",
                    transaction_id
                );
                return Ok(());
            }
        };

        // Normalize + allowlist: only the tracked channels are indexed.
        let channel = channel_raw.trim().to_lowercase();
        if !BROADCAST_CHANNELS.contains(&channel.as_str()) {
            info!(
                "Broadcast {} on non-tracked channel '{}', skipping",
                transaction_id, channel
            );
            return Ok(());
        }

        if content.chars().count() > MAX_BROADCAST_CONTENT_CHARS {
            info!(
                "Broadcast {} exceeds max content length, skipping",
                transaction_id
            );
            return Ok(());
        }

        // Sender = the self-send address (broadcasts pay back to the author). simply-kaspa's
        // addresses_transactions stores the bech32 payload without the hrp; prefix it. Rows
        // commit together with the transaction row, so the address is present by now.
        let transaction_id_bytes = hex::decode(transaction_id)?;
        let addr: Option<String> = sqlx::query_scalar(
            "SELECT address FROM addresses_transactions WHERE transaction_id = $1 LIMIT 1",
        )
        .bind(&transaction_id_bytes)
        .fetch_optional(&self.db_pool)
        .await?;
        let sender_address = match addr {
            Some(a) => format!("{}:{}", self.address_hrp(), a),
            None => {
                warn!(
                    "Broadcast {} has no indexed sender address, skipping",
                    transaction_id
                );
                return Ok(());
            }
        };

        let block_time = transaction.block_time.unwrap_or(0);
        let result = sqlx::query(
            r#"
            INSERT INTO kachat_broadcasts (
                transaction_id, block_time, channel, sender_address, content
            ) VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (transaction_id) DO NOTHING
            "#,
        )
        .bind(&transaction_id_bytes)
        .bind(block_time)
        .bind(&channel)
        .bind(&sender_address)
        .bind(content)
        .execute(&self.db_pool)
        .await?;

        if result.rows_affected() == 0 {
            info!("Broadcast {} already exists, skipping", transaction_id);
        } else {
            info!(
                "Saved KaChat broadcast {} on #{} from {}",
                transaction_id, channel, sender_address
            );
        }
        Ok(())
    }

    /// Save K post to database
    pub async fn save_k_post_to_database(
        &self,
        transaction: &Transaction,
        k_post: KPost,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's the base64 message + mentioned_pubkeys JSON
        let mentioned_pubkeys_json_str =
            serde_json::to_string(&k_post.mentioned_pubkeys).unwrap_or_else(|_| "[]".to_string());
        let message_to_verify = format!(
            "{}:{}",
            k_post.base64_encoded_message, mentioned_pubkeys_json_str
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_post.sender_signature,
            &k_post.sender_pubkey,
        ) {
            error!("Invalid signature for post {}, skipping", transaction_id);
            return Ok(()); // Skip posts with invalid signatures
        }

        // KaChat exclusivity + text-only content policy (fork addition).
        if let Err(reason) = validate_kachat_message(&k_post.base64_encoded_message) {
            info!("Post {} rejected ({}), skipping", transaction_id, reason);
            return Ok(());
        }

        // Extract block time
        let block_time = transaction.block_time.unwrap_or(0);

        // Convert hex strings to bytea for database storage
        let transaction_id_bytes = hex::decode(transaction_id)?;
        let sender_pubkey_bytes = hex::decode(&k_post.sender_pubkey)?;
        let sender_signature_bytes = hex::decode(&k_post.sender_signature)?;

        // Extract hashtags from the message
        let hashtags = extract_hashtags_from_base64(&k_post.base64_encoded_message);

        // Single query to insert post and all mentions/hashtags using CTE
        if k_post.mentioned_pubkeys.is_empty() {
            // No mentions - check if we have hashtags
            if hashtags.is_empty() {
                // No mentions, no hashtags - simple insert
                let result = sqlx::query(
                    r#"
                    INSERT INTO k_contents (
                        transaction_id, block_time, sender_pubkey, sender_signature,
                        base64_encoded_message, content_type, referenced_content_id
                    ) VALUES ($1, $2, $3, $4, $5, 'post', NULL)
                    ON CONFLICT (sender_signature) DO NOTHING
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_post.base64_encoded_message)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Post transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!("Saved K post: {}", transaction_id);
                }
            } else {
                // No mentions but has hashtags - use CTE to insert post + hashtags atomically
                let result = sqlx::query(
                    r#"
                    WITH post_insert AS (
                        INSERT INTO k_contents (
                            transaction_id, block_time, sender_pubkey, sender_signature,
                            base64_encoded_message, content_type, referenced_content_id
                        ) VALUES ($1, $2, $3, $4, $5, 'post', NULL)
                        ON CONFLICT (sender_signature) DO NOTHING
                        RETURNING transaction_id, block_time, sender_pubkey
                    )
                    INSERT INTO k_hashtags (sender_pubkey, content_id, block_time, hashtag)
                    SELECT pi.sender_pubkey, pi.transaction_id, pi.block_time, unnest($6::text[])
                    FROM post_insert pi
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_post.base64_encoded_message)
                .bind(&hashtags)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Post transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!(
                        "Saved K post with {} hashtags: {}",
                        hashtags.len(),
                        transaction_id
                    );
                }
            }
        } else {
            // Has mentions - check if we also have hashtags
            // Convert mentioned pubkeys to bytea
            let mentioned_pubkeys_bytes: Result<Vec<Vec<u8>>, _> = k_post
                .mentioned_pubkeys
                .iter()
                .map(|pk| hex::decode(pk))
                .collect();
            let mentioned_pubkeys_bytes = mentioned_pubkeys_bytes?;

            if hashtags.is_empty() {
                // Has mentions but no hashtags - CTE with post + mentions
                let result = sqlx::query(
                    r#"
                    WITH post_insert AS (
                        INSERT INTO k_contents (
                            transaction_id, block_time, sender_pubkey, sender_signature,
                            base64_encoded_message, content_type, referenced_content_id
                        ) VALUES ($1, $2, $3, $4, $5, 'post', NULL)
                        ON CONFLICT (sender_signature) DO NOTHING
                        RETURNING transaction_id, block_time, sender_pubkey
                    )
                    INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
                    SELECT pi.transaction_id, 'post', unnest($6::bytea[]), pi.block_time, pi.sender_pubkey
                    FROM post_insert pi
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_post.base64_encoded_message)
                .bind(&mentioned_pubkeys_bytes)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Post transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!("Saved K post: {}", transaction_id);
                }
            } else {
                // Has both mentions AND hashtags - extended CTE with post + mentions + hashtags
                let result = sqlx::query(
                    r#"
                    WITH post_insert AS (
                        INSERT INTO k_contents (
                            transaction_id, block_time, sender_pubkey, sender_signature,
                            base64_encoded_message, content_type, referenced_content_id
                        ) VALUES ($1, $2, $3, $4, $5, 'post', NULL)
                        ON CONFLICT (sender_signature) DO NOTHING
                        RETURNING transaction_id, block_time, sender_pubkey
                    ),
                    mentions_insert AS (
                        INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
                        SELECT pi.transaction_id, 'post', unnest($6::bytea[]), pi.block_time, pi.sender_pubkey
                        FROM post_insert pi
                        RETURNING 1
                    )
                    INSERT INTO k_hashtags (sender_pubkey, content_id, block_time, hashtag)
                    SELECT pi.sender_pubkey, pi.transaction_id, pi.block_time, unnest($7::text[])
                    FROM post_insert pi
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_post.base64_encoded_message)
                .bind(&mentioned_pubkeys_bytes)
                .bind(&hashtags)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Post transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!(
                        "Saved K post with {} mentions and {} hashtags: {}",
                        mentioned_pubkeys_bytes.len(),
                        hashtags.len(),
                        transaction_id
                    );
                }
            }
        }
        Ok(())
    }

    /// Save K reply to database
    pub async fn save_k_reply_to_database(
        &self,
        transaction: &Transaction,
        k_reply: KReply,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's post_id + base64_message + mentioned_pubkeys JSON
        let mentioned_pubkeys_json_str =
            serde_json::to_string(&k_reply.mentioned_pubkeys).unwrap_or_else(|_| "[]".to_string());
        let message_to_verify = format!(
            "{}:{}:{}",
            k_reply.post_id, k_reply.base64_encoded_message, mentioned_pubkeys_json_str
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_reply.sender_signature,
            &k_reply.sender_pubkey,
        ) {
            error!("Invalid signature for reply {}, skipping", transaction_id);
            return Ok(()); // Skip replies with invalid signatures
        }

        // KaChat exclusivity + text-only content policy (fork addition).
        if let Err(reason) = validate_kachat_message(&k_reply.base64_encoded_message) {
            info!("Reply {} rejected ({}), skipping", transaction_id, reason);
            return Ok(());
        }

        // Store values we need for logging before they're moved
        let post_id_for_log = k_reply.post_id.clone();

        // Extract block time
        let block_time = transaction.block_time.unwrap_or(0);

        // Convert hex strings to bytea for database storage
        let transaction_id_bytes = hex::decode(transaction_id)?;
        let sender_pubkey_bytes = hex::decode(&k_reply.sender_pubkey)?;
        let sender_signature_bytes = hex::decode(&k_reply.sender_signature)?;
        let post_id_bytes = hex::decode(&k_reply.post_id)?;

        // Extract hashtags from the message
        let hashtags = extract_hashtags_from_base64(&k_reply.base64_encoded_message);

        // Single query to insert reply and all mentions/hashtags using CTE
        if k_reply.mentioned_pubkeys.is_empty() {
            // No mentions - check if we have hashtags
            if hashtags.is_empty() {
                // No mentions, no hashtags - simple insert
                let result = sqlx::query(
                    r#"
                    INSERT INTO k_contents (
                        transaction_id, block_time, sender_pubkey, sender_signature,
                        base64_encoded_message, content_type, referenced_content_id
                    ) VALUES ($1, $2, $3, $4, $5, 'reply', $6)
                    ON CONFLICT (sender_signature) DO NOTHING
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_reply.base64_encoded_message)
                .bind(&post_id_bytes)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Reply transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!("Saved K reply: {} -> {}", transaction_id, post_id_for_log);
                }
            } else {
                // No mentions but has hashtags - use CTE to insert reply + hashtags atomically
                let result = sqlx::query(
                    r#"
                    WITH reply_insert AS (
                        INSERT INTO k_contents (
                            transaction_id, block_time, sender_pubkey, sender_signature,
                            base64_encoded_message, content_type, referenced_content_id
                        ) VALUES ($1, $2, $3, $4, $5, 'reply', $6)
                        ON CONFLICT (sender_signature) DO NOTHING
                        RETURNING transaction_id, block_time, sender_pubkey
                    )
                    INSERT INTO k_hashtags (sender_pubkey, content_id, block_time, hashtag)
                    SELECT ri.sender_pubkey, ri.transaction_id, ri.block_time, unnest($7::text[])
                    FROM reply_insert ri
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_reply.base64_encoded_message)
                .bind(&post_id_bytes)
                .bind(&hashtags)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Reply transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!(
                        "Saved K reply with {} hashtags: {} -> {}",
                        hashtags.len(),
                        transaction_id,
                        post_id_for_log
                    );
                }
            }
        } else {
            // Has mentions - check if we also have hashtags
            // Convert mentioned pubkeys to bytea
            let mentioned_pubkeys_bytes: Result<Vec<Vec<u8>>, _> = k_reply
                .mentioned_pubkeys
                .iter()
                .map(|pk| hex::decode(pk))
                .collect();
            let mentioned_pubkeys_bytes = mentioned_pubkeys_bytes?;

            if hashtags.is_empty() {
                // Has mentions but no hashtags - CTE with reply + mentions
                let result = sqlx::query(
                    r#"
                    WITH reply_insert AS (
                        INSERT INTO k_contents (
                            transaction_id, block_time, sender_pubkey, sender_signature,
                            base64_encoded_message, content_type, referenced_content_id
                        ) VALUES ($1, $2, $3, $4, $5, 'reply', $6)
                        ON CONFLICT (sender_signature) DO NOTHING
                        RETURNING transaction_id, block_time, sender_pubkey
                    )
                    INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
                    SELECT ri.transaction_id, 'reply', unnest($7::bytea[]), ri.block_time, ri.sender_pubkey
                    FROM reply_insert ri
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_reply.base64_encoded_message)
                .bind(&post_id_bytes)
                .bind(&mentioned_pubkeys_bytes)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Reply transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!("Saved K reply: {} -> {}", transaction_id, post_id_for_log);
                }
            } else {
                // Has both mentions AND hashtags - extended CTE with reply + mentions + hashtags
                let result = sqlx::query(
                    r#"
                    WITH reply_insert AS (
                        INSERT INTO k_contents (
                            transaction_id, block_time, sender_pubkey, sender_signature,
                            base64_encoded_message, content_type, referenced_content_id
                        ) VALUES ($1, $2, $3, $4, $5, 'reply', $6)
                        ON CONFLICT (sender_signature) DO NOTHING
                        RETURNING transaction_id, block_time, sender_pubkey
                    ),
                    mentions_insert AS (
                        INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
                        SELECT ri.transaction_id, 'reply', unnest($7::bytea[]), ri.block_time, ri.sender_pubkey
                        FROM reply_insert ri
                        RETURNING 1
                    )
                    INSERT INTO k_hashtags (sender_pubkey, content_id, block_time, hashtag)
                    SELECT ri.sender_pubkey, ri.transaction_id, ri.block_time, unnest($8::text[])
                    FROM reply_insert ri
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_reply.base64_encoded_message)
                .bind(&post_id_bytes)
                .bind(&mentioned_pubkeys_bytes)
                .bind(&hashtags)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Reply transaction {} already exists, skipping",
                        transaction_id
                    );
                } else {
                    info!(
                        "Saved K reply with {} mentions and {} hashtags: {} -> {}",
                        mentioned_pubkeys_bytes.len(),
                        hashtags.len(),
                        transaction_id,
                        post_id_for_log
                    );
                }
            }
        }
        Ok(())
    }

    /// Save K quote to database
    pub async fn save_k_quote_to_database(
        &self,
        transaction: &Transaction,
        k_quote: KQuote,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's content_id + base64_message + mentioned_pubkey
        let message_to_verify = format!(
            "{}:{}:{}",
            k_quote.content_id, k_quote.base64_encoded_message, k_quote.mentioned_pubkey
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_quote.sender_signature,
            &k_quote.sender_pubkey,
        ) {
            error!("Invalid signature for quote {}, skipping", transaction_id);
            return Ok(()); // Skip quotes with invalid signatures
        }

        // KaChat exclusivity + text-only content policy (fork addition).
        // A plain repost is a quote whose message is exactly the marker (empty body), so it
        // still passes validation.
        if let Err(reason) = validate_kachat_message(&k_quote.base64_encoded_message) {
            info!("Quote {} rejected ({}), skipping", transaction_id, reason);
            return Ok(());
        }

        // Store values we need for logging before they're moved
        let content_id_for_log = k_quote.content_id.clone();
        let mentioned_pubkey_for_log = k_quote.mentioned_pubkey.clone();

        // Extract block time
        let block_time = transaction.block_time.unwrap_or(0);

        // Convert hex strings to bytea for database storage
        let transaction_id_bytes = hex::decode(transaction_id)?;
        let sender_pubkey_bytes = hex::decode(&k_quote.sender_pubkey)?;
        let sender_signature_bytes = hex::decode(&k_quote.sender_signature)?;
        let content_id_bytes = hex::decode(&k_quote.content_id)?;
        let mentioned_pubkey_bytes = hex::decode(&k_quote.mentioned_pubkey)?;

        // Extract hashtags from the message
        let hashtags = extract_hashtags_from_base64(&k_quote.base64_encoded_message);

        // Single query to insert quote, mention, and hashtags using CTE
        if hashtags.is_empty() {
            // No hashtags - CTE with quote + mention only
            let result = sqlx::query(
                r#"
                WITH quote_insert AS (
                    INSERT INTO k_contents (
                        transaction_id, block_time, sender_pubkey, sender_signature,
                        base64_encoded_message, content_type, referenced_content_id
                    ) VALUES ($1, $2, $3, $4, $5, 'quote', $6)
                    ON CONFLICT (sender_signature) DO NOTHING
                    RETURNING transaction_id, block_time, sender_pubkey
                )
                INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
                SELECT qi.transaction_id, 'quote', $7, qi.block_time, qi.sender_pubkey
                FROM quote_insert qi
                "#,
            )
            .bind(&transaction_id_bytes)
            .bind(block_time)
            .bind(&sender_pubkey_bytes)
            .bind(&sender_signature_bytes)
            .bind(&k_quote.base64_encoded_message)
            .bind(&content_id_bytes)
            .bind(&mentioned_pubkey_bytes)
            .execute(&self.db_pool)
            .await?;

            if result.rows_affected() == 0 {
                info!(
                    "Quote transaction {} already exists, skipping",
                    transaction_id
                );
            } else {
                info!(
                    "Saved K quote: {} -> {} (mentioned: {})",
                    transaction_id, content_id_for_log, mentioned_pubkey_for_log
                );
            }
        } else {
            // Has hashtags - extended CTE with quote + mention + hashtags
            let result = sqlx::query(
                r#"
                WITH quote_insert AS (
                    INSERT INTO k_contents (
                        transaction_id, block_time, sender_pubkey, sender_signature,
                        base64_encoded_message, content_type, referenced_content_id
                    ) VALUES ($1, $2, $3, $4, $5, 'quote', $6)
                    ON CONFLICT (sender_signature) DO NOTHING
                    RETURNING transaction_id, block_time, sender_pubkey
                ),
                mentions_insert AS (
                    INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
                    SELECT qi.transaction_id, 'quote', $7, qi.block_time, qi.sender_pubkey
                    FROM quote_insert qi
                    RETURNING 1
                )
                INSERT INTO k_hashtags (sender_pubkey, content_id, block_time, hashtag)
                SELECT qi.sender_pubkey, qi.transaction_id, qi.block_time, unnest($8::text[])
                FROM quote_insert qi
                "#,
            )
            .bind(&transaction_id_bytes)
            .bind(block_time)
            .bind(&sender_pubkey_bytes)
            .bind(&sender_signature_bytes)
            .bind(&k_quote.base64_encoded_message)
            .bind(&content_id_bytes)
            .bind(&mentioned_pubkey_bytes)
            .bind(&hashtags)
            .execute(&self.db_pool)
            .await?;

            if result.rows_affected() == 0 {
                info!(
                    "Quote transaction {} already exists, skipping",
                    transaction_id
                );
            } else {
                info!(
                    "Saved K quote with {} hashtags: {} -> {} (mentioned: {})",
                    hashtags.len(),
                    transaction_id,
                    content_id_for_log,
                    mentioned_pubkey_for_log
                );
            }
        }
        Ok(())
    }

    /// Save K broadcast to database
    pub async fn save_k_broadcast_to_database(
        &self,
        transaction: &Transaction,
        k_broadcast: KBroadcast,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's nickname + profile_image + message
        let profile_image_str = k_broadcast
            .base64_encoded_profile_image
            .as_deref()
            .unwrap_or("");
        let message_to_verify = format!(
            "{}:{}:{}",
            k_broadcast.base64_encoded_nickname,
            profile_image_str,
            k_broadcast.base64_encoded_message
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_broadcast.sender_signature,
            &k_broadcast.sender_pubkey,
        ) {
            error!(
                "Invalid signature for broadcast {}, skipping",
                transaction_id
            );
            return Ok(()); // Skip broadcasts with invalid signatures
        }

        // Convert hex strings to bytea for database storage
        let transaction_id_bytes = hex::decode(transaction_id)?;
        let sender_pubkey_bytes = hex::decode(&k_broadcast.sender_pubkey)?;
        let sender_signature_bytes = hex::decode(&k_broadcast.sender_signature)?;

        // Use a single query to delete existing records and insert the new one atomically (skip if transaction already exists)
        let result = sqlx::query(
            r#"
            WITH deleted AS (
                DELETE FROM k_broadcasts
                WHERE sender_pubkey = $3 AND transaction_id != $1
                RETURNING transaction_id
            )
            INSERT INTO k_broadcasts (
                transaction_id, block_time, sender_pubkey, sender_signature,
                base64_encoded_nickname, base64_encoded_profile_image, base64_encoded_message
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (transaction_id) DO NOTHING
            "#,
        )
        .bind(&transaction_id_bytes)
        .bind(transaction.block_time.unwrap_or(0))
        .bind(&sender_pubkey_bytes)
        .bind(&sender_signature_bytes)
        .bind(k_broadcast.base64_encoded_nickname)
        .bind(k_broadcast.base64_encoded_profile_image)
        .bind(k_broadcast.base64_encoded_message)
        .execute(&self.db_pool)
        .await?;

        if result.rows_affected() == 0 {
            info!(
                "Broadcast transaction {} already exists, skipping",
                transaction_id
            );
        } else {
            info!(
                "Saved K broadcast: {} (replaced any existing broadcasts for sender)",
                transaction_id
            );
        }
        Ok(())
    }

    /// Save K vote to database
    pub async fn save_k_vote_to_database(
        &self,
        transaction: &Transaction,
        k_vote: KVote,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's post_id + vote + mentioned_pubkey
        let message_to_verify = format!(
            "{}:{}:{}",
            k_vote.post_id, k_vote.vote, k_vote.mentioned_pubkey
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_vote.sender_signature,
            &k_vote.sender_pubkey,
        ) {
            error!("Invalid signature for vote {}, skipping", transaction_id);
            return Ok(()); // Skip votes with invalid signatures
        }

        // Removal counter-action (fork addition): 'unvote' withdraws the sender's prior
        // upvote/downvote on this post. Delete the matching vote row (and its notification
        // mention); up/down vote counts are computed live, so they drop automatically.
        if k_vote.vote == "unvote" {
            let sender_pubkey_bytes = hex::decode(&k_vote.sender_pubkey)?;
            let post_id_bytes = hex::decode(&k_vote.post_id)?;
            let delete_result = sqlx::query(
                r#"
                WITH deleted_votes AS (
                    DELETE FROM k_votes
                    WHERE post_id = $1 AND sender_pubkey = $2
                    RETURNING transaction_id
                )
                DELETE FROM k_mentions
                WHERE content_type = 'vote'
                  AND content_id IN (SELECT transaction_id FROM deleted_votes)
                "#,
            )
            .bind(&post_id_bytes)
            .bind(&sender_pubkey_bytes)
            .execute(&self.db_pool)
            .await?;
            info!(
                "Processed K unvote: {} un-voted {} (removed prior vote, {} mention rows)",
                hex::encode(&sender_pubkey_bytes),
                k_vote.post_id,
                delete_result.rows_affected()
            );
            return Ok(());
        }

        // Store values we need for logging before they're moved
        let post_id_for_log = k_vote.post_id.clone();
        let vote_for_log = k_vote.vote.clone();

        // Extract block time
        let block_time = transaction.block_time.unwrap_or(0);

        // Convert hex strings to bytea for database storage
        let transaction_id_bytes = hex::decode(transaction_id)?;
        let sender_pubkey_bytes = hex::decode(&k_vote.sender_pubkey)?;
        let sender_signature_bytes = hex::decode(&k_vote.sender_signature)?;
        let post_id_bytes = hex::decode(&k_vote.post_id)?;
        let mentioned_pubkey_bytes = hex::decode(&k_vote.mentioned_pubkey)?;

        // Fresh KaChat-only network: index a vote only if it targets indexed KaChat content.
        // This keeps K-network engagement (votes on K-website posts, which are marker-filtered
        // and never stored) out of the database entirely. KaChat posts are indexed before
        // their votes arrive in normal operation, so legitimate KaChat votes are kept.
        let target_is_kachat =
            sqlx::query("SELECT 1 FROM k_contents WHERE transaction_id = $1 LIMIT 1")
                .bind(&post_id_bytes)
                .fetch_optional(&self.db_pool)
                .await?
                .is_some();
        if !target_is_kachat {
            info!(
                "Vote {} targets non-KaChat content {}, skipping",
                transaction_id, k_vote.post_id
            );
            return Ok(());
        }

        // Single query to insert vote and mention using CTE (skip if already exists)
        let result = sqlx::query(
            r#"
            WITH vote_insert AS (
                INSERT INTO k_votes (
                    transaction_id, block_time, sender_pubkey, sender_signature,
                    post_id, vote
                ) VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (sender_signature) DO NOTHING
                RETURNING transaction_id, block_time, sender_pubkey
            )
            INSERT INTO k_mentions (content_id, content_type, mentioned_pubkey, block_time, sender_pubkey)
            SELECT vi.transaction_id, 'vote', $7, vi.block_time, vi.sender_pubkey
            FROM vote_insert vi
            "#,
        )
        .bind(&transaction_id_bytes)
        .bind(block_time)
        .bind(&sender_pubkey_bytes)
        .bind(&sender_signature_bytes)
        .bind(&post_id_bytes)
        .bind(k_vote.vote)
        .bind(&mentioned_pubkey_bytes)
        .execute(&self.db_pool)
        .await?;

        if result.rows_affected() == 0 {
            info!(
                "Vote transaction {} already exists, skipping",
                transaction_id
            );
        } else {
            info!(
                "Saved K vote: {} -> {} ({})",
                transaction_id, post_id_for_log, vote_for_log
            );
        }
        Ok(())
    }

    /// Process K unquote action (fork addition): remove the sender's prior quote/repost of a
    /// content id. Mirrors unfollow/unblock. quotesCount is computed live, so deleting the
    /// k_contents row(s) makes the count drop. k_hashtags cascade via FK; k_mentions are
    /// deleted explicitly (no FK).
    pub async fn process_k_unquote_in_database(
        &self,
        transaction: &Transaction,
        k_unquote: KUnquote,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Signing string is just the referenced content id.
        if !self.verify_kaspa_signature(
            &k_unquote.content_id,
            &k_unquote.sender_signature,
            &k_unquote.sender_pubkey,
        ) {
            error!(
                "Invalid signature for unquote action {}, skipping",
                transaction_id
            );
            return Ok(());
        }

        let sender_pubkey_bytes = hex::decode(&k_unquote.sender_pubkey)?;
        let content_id_bytes = hex::decode(&k_unquote.content_id)?;

        // Delete the sender's quote/repost row(s) referencing this content, plus their
        // notification mentions. Hashtags are removed by ON DELETE CASCADE on k_hashtags.
        let delete_result = sqlx::query(
            r#"
            WITH deleted_contents AS (
                DELETE FROM k_contents
                WHERE referenced_content_id = $1
                  AND sender_pubkey = $2
                  AND content_type IN ('quote', 'repost')
                RETURNING transaction_id
            )
            DELETE FROM k_mentions
            WHERE content_type IN ('quote', 'repost')
              AND content_id IN (SELECT transaction_id FROM deleted_contents)
            "#,
        )
        .bind(&content_id_bytes)
        .bind(&sender_pubkey_bytes)
        .execute(&self.db_pool)
        .await?;

        info!(
            "Processed K unquote: {} un-quoted {} (removed quote/repost, {} mention rows)",
            hex::encode(&sender_pubkey_bytes),
            k_unquote.content_id,
            delete_result.rows_affected()
        );
        Ok(())
    }

    /// Process K block action (block/unblock) in database
    pub async fn process_k_block_in_database(
        &self,
        transaction: &Transaction,
        k_block: KBlock,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's blocking_action + blocked_user_pubkey
        let message_to_verify = format!(
            "{}:{}",
            k_block.blocking_action, k_block.blocked_user_pubkey
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_block.sender_signature,
            &k_block.sender_pubkey,
        ) {
            error!(
                "Invalid signature for block action {}, skipping",
                transaction_id
            );
            return Ok(()); // Skip block actions with invalid signatures
        }

        // Extract block time
        let block_time = transaction.block_time.unwrap_or(0);

        // Convert hex strings to bytea for database storage
        let sender_pubkey_bytes = hex::decode(&k_block.sender_pubkey)?;
        let blocked_user_pubkey_bytes = hex::decode(&k_block.blocked_user_pubkey)?;

        match k_block.blocking_action.as_str() {
            "block" => {
                let transaction_id_bytes = hex::decode(transaction_id)?;
                let sender_signature_bytes = hex::decode(&k_block.sender_signature)?;

                // Insert block record (skip if same sender already blocked this user)
                let result = sqlx::query(
                    r#"
                    INSERT INTO k_blocks (
                        transaction_id, block_time, sender_pubkey, sender_signature,
                        blocking_action, blocked_user_pubkey
                    ) VALUES ($1, $2, $3, $4, $5, $6)
                    ON CONFLICT (sender_pubkey, blocked_user_pubkey)
                    DO NOTHING
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_block.blocking_action)
                .bind(&blocked_user_pubkey_bytes)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Block already exists: {} already blocked {} (keeping original), skipping",
                        hex::encode(&sender_pubkey_bytes),
                        hex::encode(&blocked_user_pubkey_bytes)
                    );
                } else {
                    info!(
                        "Saved K block: {} blocked {}",
                        hex::encode(&sender_pubkey_bytes),
                        hex::encode(&blocked_user_pubkey_bytes)
                    );
                }
            }
            "unblock" => {
                // Delete any existing "block" record for the same sender and blocked user
                let delete_result = sqlx::query(
                    r#"
                    DELETE FROM k_blocks
                    WHERE sender_pubkey = $1
                    AND blocked_user_pubkey = $2
                    AND blocking_action = 'block'
                    "#,
                )
                .bind(&sender_pubkey_bytes)
                .bind(&blocked_user_pubkey_bytes)
                .execute(&self.db_pool)
                .await?;

                info!(
                    "Processed K unblock: {} unblocked {} (deleted {} existing block records)",
                    hex::encode(&sender_pubkey_bytes),
                    hex::encode(&blocked_user_pubkey_bytes),
                    delete_result.rows_affected()
                );
            }
            _ => {
                error!("Invalid blocking_action: {}", k_block.blocking_action);
                return Err(anyhow::anyhow!(
                    "Invalid blocking_action: {}",
                    k_block.blocking_action
                ));
            }
        }

        Ok(())
    }

    /// Process K follow action (follow/unfollow) in database
    pub async fn process_k_follow_in_database(
        &self,
        transaction: &Transaction,
        k_follow: KFollow,
    ) -> Result<()> {
        let transaction_id = &transaction.transaction_id;

        // Construct the message to verify - it's following_action + followed_user_pubkey
        let message_to_verify = format!(
            "{}:{}",
            k_follow.following_action, k_follow.followed_user_pubkey
        );

        // Verify the signature
        if !self.verify_kaspa_signature(
            &message_to_verify,
            &k_follow.sender_signature,
            &k_follow.sender_pubkey,
        ) {
            error!(
                "Invalid signature for follow action {}, skipping",
                transaction_id
            );
            return Ok(()); // Skip follow actions with invalid signatures
        }

        // Extract block time
        let block_time = transaction.block_time.unwrap_or(0);

        // Convert hex strings to bytea for database storage
        let sender_pubkey_bytes = hex::decode(&k_follow.sender_pubkey)?;
        let followed_user_pubkey_bytes = hex::decode(&k_follow.followed_user_pubkey)?;

        match k_follow.following_action.as_str() {
            "follow" => {
                let transaction_id_bytes = hex::decode(transaction_id)?;
                let sender_signature_bytes = hex::decode(&k_follow.sender_signature)?;

                // Fresh KaChat-only network: index a follow only if it involves a KaChat
                // identity — i.e. the follower OR the followed user has authored KaChat
                // content. Pure K-network follows (neither party posts on KaChat) never enter
                // the DB. (Unfollow below always runs its delete regardless.)
                let kachat_related = sqlx::query(
                    "SELECT 1 FROM k_contents WHERE sender_pubkey = $1 OR sender_pubkey = $2 LIMIT 1",
                )
                .bind(&sender_pubkey_bytes)
                .bind(&followed_user_pubkey_bytes)
                .fetch_optional(&self.db_pool)
                .await?
                .is_some();
                if !kachat_related {
                    info!(
                        "Follow {} involves no KaChat identity, skipping",
                        transaction_id
                    );
                    return Ok(());
                }

                // Insert follow record (skip if same sender already follows this user)
                let result = sqlx::query(
                    r#"
                    INSERT INTO k_follows (
                        transaction_id, block_time, sender_pubkey, sender_signature,
                        following_action, followed_user_pubkey
                    ) VALUES ($1, $2, $3, $4, $5, $6)
                    ON CONFLICT (sender_pubkey, followed_user_pubkey)
                    DO NOTHING
                    "#,
                )
                .bind(&transaction_id_bytes)
                .bind(block_time)
                .bind(&sender_pubkey_bytes)
                .bind(&sender_signature_bytes)
                .bind(&k_follow.following_action)
                .bind(&followed_user_pubkey_bytes)
                .execute(&self.db_pool)
                .await?;

                if result.rows_affected() == 0 {
                    info!(
                        "Follow already exists: {} already follows {} (keeping original), skipping",
                        hex::encode(&sender_pubkey_bytes),
                        hex::encode(&followed_user_pubkey_bytes)
                    );
                } else {
                    info!(
                        "Saved K follow: {} followed {}",
                        hex::encode(&sender_pubkey_bytes),
                        hex::encode(&followed_user_pubkey_bytes)
                    );
                }
            }
            "unfollow" => {
                // Delete any existing "follow" record for the same sender and followed user
                let delete_result = sqlx::query(
                    r#"
                    DELETE FROM k_follows
                    WHERE sender_pubkey = $1
                    AND followed_user_pubkey = $2
                    AND following_action = 'follow'
                    "#,
                )
                .bind(&sender_pubkey_bytes)
                .bind(&followed_user_pubkey_bytes)
                .execute(&self.db_pool)
                .await?;

                info!(
                    "Processed K unfollow: {} unfollowed {} (deleted {} existing follow records)",
                    hex::encode(&sender_pubkey_bytes),
                    hex::encode(&followed_user_pubkey_bytes),
                    delete_result.rows_affected()
                );
            }
            _ => {
                error!("Invalid following_action: {}", k_follow.following_action);
                return Err(anyhow::anyhow!(
                    "Invalid following_action: {}",
                    k_follow.following_action
                ));
            }
        }

        Ok(())
    }
}
