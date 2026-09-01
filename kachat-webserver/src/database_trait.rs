use crate::models::{
    BroadcastMessage, ContentRecord, EngagementActor, KBroadcastRecord, KPostRecord, KReplyRecord,
    NotificationContentRecord, PaginationMetadata,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::result::Result as StdResult;

pub type DatabaseResult<T> = StdResult<T, DatabaseError>;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DatabaseError {
    ConnectionError(String),
    QueryError(String),
    SerializationError(String),
    NotFound,
    InvalidInput(String),
}

impl std::fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DatabaseError::ConnectionError(msg) => write!(f, "Connection error: {}", msg),
            DatabaseError::QueryError(msg) => write!(f, "Query error: {}", msg),
            DatabaseError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            DatabaseError::NotFound => write!(f, "Record not found"),
            DatabaseError::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
        }
    }
}

impl std::error::Error for DatabaseError {}

#[derive(Debug, Clone)]
pub struct QueryOptions {
    pub limit: Option<u64>,
    pub before: Option<String>, // Compound cursors like "timestamp_id"
    pub after: Option<String>,  // Compound cursors like "timestamp_id"
    pub sort_descending: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            limit: None,
            before: None,
            after: None,
            sort_descending: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaginatedResult<T> {
    pub items: Vec<T>,
    pub pagination: PaginationMetadata,
}

#[async_trait]
#[allow(dead_code)]
pub trait DatabaseInterface: Send + Sync {
    // Post operations (optimized versions with metadata)

    // User operations
    async fn get_all_users(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<(KBroadcastRecord, bool, bool)>>;

    async fn get_most_active_users(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
        from_time_millis: u64,
        to_time_millis: u64,
    ) -> DatabaseResult<PaginatedResult<(KBroadcastRecord, bool, bool, i64)>>;

    async fn search_users(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
        searched_user_pubkey: Option<String>,
        searched_user_nickname: Option<String>,
    ) -> DatabaseResult<PaginatedResult<(KBroadcastRecord, bool, bool)>>;

    async fn get_user_details(
        &self,
        user_public_key: &str,
        requester_pubkey: &str,
    ) -> DatabaseResult<Option<(KBroadcastRecord, bool, bool, i64, i64, i64)>>;

    async fn get_blocked_users_by_requester(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KBroadcastRecord>>;

    async fn get_followed_users_by_requester(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KBroadcastRecord>>;

    async fn get_users_following(
        &self,
        requester_pubkey: &str,
        user_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<(KBroadcastRecord, bool)>>;

    async fn get_users_followers(
        &self,
        requester_pubkey: &str,
        user_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<(KBroadcastRecord, bool)>>;

    // NEW: k_contents table - Get all posts using unified content table (excludes blocked users)
    async fn get_all_posts(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KPostRecord>>;

    // NEW: k_contents table - Get content (posts, replies, quotes) from followed users (excludes blocked users)
    async fn get_content_following(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KPostRecord>>;

    // NEW: k_contents table - Get contents mentioning a specific user using unified content table (excludes blocked users)
    async fn get_contents_mentioning_user(
        &self,
        user_public_key: &str,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<ContentRecord>>;

    // NEW: k_contents table - Get replies by post ID using unified content table (excludes blocked users)
    async fn get_replies_by_post_id(
        &self,
        post_id: &str,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KReplyRecord>>;

    // NEW: k_contents table - Get replies by user using unified content table (excludes blocked users)
    async fn get_replies_by_user(
        &self,
        user_public_key: &str,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KReplyRecord>>;

    // NEW: k_contents table - Get posts by user using unified content table (excludes blocked users)
    async fn get_posts_by_user(
        &self,
        user_public_key: &str,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KPostRecord>>;

    // NEW: k_contents table - Get notifications using unified content table
    async fn get_notifications(
        &self,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<NotificationContentRecord>>;

    // Fork addition: per-post actor lists — who upvoted/downvoted/reposted/quoted a post.
    // engagement_type is one of "upvote" | "downvote" | "repost" | "quote" | "all".
    async fn get_post_engagement(
        &self,
        post_id: &str,
        engagement_type: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<EngagementActor>>;

    // Fork addition: KaChat broadcasts for a tracked channel, newest-first. Returns the page
    // of messages plus whether more older rows exist. `before` filters block_time (ms).
    async fn get_broadcasts(
        &self,
        channel: &str,
        limit: u32,
        before: Option<i64>,
    ) -> DatabaseResult<(Vec<BroadcastMessage>, bool)>;

    // NEW: k_contents table - Get content by ID using unified content table
    async fn get_content_by_id(
        &self,
        content_id: &str,
        requester_pubkey: &str,
    ) -> DatabaseResult<Option<(ContentRecord, bool)>>;

    // Get count of notifications (mentions) for a user
    async fn get_notification_count(
        &self,
        requester_pubkey: &str,
        after: Option<String>,
    ) -> DatabaseResult<u64>;

    // Get count of users (broadcasts in k_broadcasts table)
    async fn get_users_count(&self) -> DatabaseResult<u64>;

    // Get network type from k_vars table
    async fn get_network(&self) -> DatabaseResult<String>;

    // Get database statistics
    async fn get_stats(&self) -> DatabaseResult<DatabaseStats>;

    // Hashtag operations

    // Get content containing a specific hashtag
    async fn get_hashtag_content(
        &self,
        hashtag: &str,
        requester_pubkey: &str,
        options: QueryOptions,
    ) -> DatabaseResult<PaginatedResult<KPostRecord>>;

    // Get trending hashtags within a time window
    // Returns: Vec<(hashtag: String, usage_count: u64)>
    async fn get_trending_hashtags(
        &self,
        from_time: u64,
        to_time: u64,
        limit: u32,
    ) -> DatabaseResult<Vec<(String, u64)>>;

    // --- Post translation (/translate endpoint) ---

    // The server's own copy of a post's message (raw stored base64, marker not stripped), by txid.
    // None if the post is not in the indexer's k_contents. Used to translate the VERIFIED text and
    // never trust request-supplied text under a txid.
    async fn get_post_base64_message(&self, tx_id_hex: &str) -> DatabaseResult<Option<String>>;

    // Cached translation of (txid, target_lang), if any. Returns (source_lang, text).
    async fn get_cached_translation(
        &self,
        tx_id_hex: &str,
        target_lang: &str,
    ) -> DatabaseResult<Option<(String, String)>>;

    // Insert a verified translation into the permanent (txid, target) cache. Idempotent.
    async fn insert_translation(
        &self,
        tx_id_hex: &str,
        target_lang: &str,
        source_lang: &str,
        text: &str,
        created_at_ms: i64,
    ) -> DatabaseResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseStats {
    pub broadcasts_count: i64,
    pub posts_count: i64,
    pub replies_count: i64,
    pub quotes_count: i64,
    pub votes_count: i64,
    pub follows_count: i64,
    pub blocks_count: i64,
}
