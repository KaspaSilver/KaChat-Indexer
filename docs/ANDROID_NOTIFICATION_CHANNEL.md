# Android pushes: follow the device's Sound & Vibration choice (2026-11-06)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaPost Android
session (`KaspaSilver/KaPosts-Android`).

On Android, a push with a `notification` block (`kaposts` and `broadcast`) is drawn by the system
when the app isn't running. The system then uses the app's default channel: KaPost's manifest names
`kachat_messages_sound_vibrate`. So these pushes always play a sound and vibrate, whatever the person
picked in Settings > Notifications > Sound & Vibration. Banners the app draws itself already use the
right channel.

Android can't change a notification's sound or vibration after it's posted. The only way is for the
push to name the channel, in FCM v1's `android.notification.channel_id`. The server doesn't know the
device's choice today, so the app has to send it at registration.

## 1. Registration: an optional `android_channel_id`

Add to `DeviceRegistration` (`push.rs`):

```rust
// Android only: the notification channel the device's Sound & Vibration choice maps to, for
// system-drawn pushes (FCM android.notification.channel_id). Outside the auth preimage, like
// every optional field added after LegacyV1. Value only.
#[serde(default)]
pub android_channel_id: Option<String>,
```

- **Accept only these four values:**
  - `kachat_messages_sound_vibrate`
  - `kachat_messages_sound_only`
  - `kachat_messages_vibrate_only`
  - `kachat_messages_silent`

  Store anything else, or a value on a non-`android` platform, as `None`. That keeps a client from
  sending a channel id it doesn't have.
- **Absent or `None`:** what happens today, the app's default channel.
- **Other clients:** KaChat and KaPay Android use the same four channel ids, so the field works
  for them too; neither has to send it.

## 2. FCM: send the channel with the notification block

In `fcm.rs`:

- **Signature:** `send_data` takes the device's `android_channel_id: Option<&str>`.
- **`AndroidNotification`:** gains `channel_id`, serialized as `channel_id` and skipped when `None`.
- **When the block is built:** whenever `include_notification` is true and there is a tag *or* a
  channel. Today it is only built when there is a `collapse_key`, so a push with no collapse id
  would drop the channel:

```rust
notification: (include_notification && (collapse_key.is_some() || android_channel_id.is_some()))
    .then(|| AndroidNotification {
        tag: collapse_key.map(|c| c.to_string()),
        channel_id: android_channel_id.map(|c| c.to_string()),
    }),
```

Data-only pushes (DM and group, `include_notification = false`) don't change: the app draws those
itself.

## How to test

- **Unit test** next to `an_os_displayed_push_with_a_collapse_id_carries_it_as_tag`:
  - a notification push with a channel serializes
    `"notification": {"tag": "ab12", "channel_id": "kachat_messages_silent"}`;
  - with no collapse id it is still built, carrying only `channel_id`;
  - a data-only push has no `notification` block.
- **Registration test:**
  - an unknown channel id, or one sent with platform `ios`, is stored as `None`;
  - an old client with no field still registers.
- **On a phone** (by the owner):
  - pick Silent in KaPost, swipe the app away, and have someone like a post: the banner shows
    with no sound and no vibration;
  - pick Sound and vibration again: the next push plays and vibrates.

## The Android side, after this ships

KaPost Android sends `android_channel_id` from `NotificationHelper.channelFor(sound, vibration)`
and re-registers when Sound & Vibration changes. The registration fingerprint takes the channel, so
the change reaches the server. Until the server ships, the app keeps working as it does today.
