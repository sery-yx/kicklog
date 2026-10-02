# How rustlog-kick works with Kick

Rustlog-kick behaves like the Twitch version of rustlog (same HTTP API, same web interface, same ClickHouse schema, same chat commands). This page describes the parts that are specific to Kick.

## Where the chat comes from

Kick has no IRC. Its web client receives chat over a [Pusher](https://pusher.com/docs/channels/library_auth_reference/pusher-websockets-protocol/) websocket, and every chat is published on a public channel named `chatrooms.<chatroom id>.v2`. Listening needs no account, so rustlog-kick joins chats anonymously, like the Twitch version does with IRC.

This is not an official API. Kick's official API only offers webhooks for chat events, which need a public HTTPS endpoint and are limited to 1000 channels for unverified apps. The websocket is what every Kick chat tool uses and has been stable for years, but Kick may change it without notice. The Pusher key and cluster can be changed in the config if that happens.

The official API is used for what it is good at: turning slugs into user ids and back (with an app access token, see [CONFIG.md](./CONFIG.md#creating-a-kick-app)).

## Ids and names

A Kick channel has three different ids. Only the first one is used by rustlog-kick:

| Id | Example (xQc) | Example (Amouranth) | Used for |
|----|---------------|---------------------|----------|
| user id of the broadcaster | `676` | `7183419` | **rustlog's channel id** and user id. Same id space as chatters (`sender.id`), the official API (`broadcaster_user_id`) and 7TV. |
| channel id | `668` | `7088698` | not used |
| chatroom id | `668` | `7022952` | subscribing to the websocket channel |

- The **login** of a user or channel is its **slug** (`some-user`), the **display name** is its username (`Some_User`). A slug is usually the lowercase username with `_` replaced by `-`, but not always, so rustlog always stores the slug Kick sends with the message.
- In URLs both spellings work: `/channel/some_user` and `/channel/some-user` are looked up.

### Finding ids

You normally do not need ids: the `channels` list in the config, the admin API and the `!rustlog join` chat command accept **slugs as well as user ids**. To look up the id of a logged channel, use `GET /channels`, which lists the name and `userID` of every channel. For other channels, Kick's website API shows it: `https://kick.com/api/v2/channels/<slug>` contains `user_id`.

## Chatroom ids

To subscribe to a chat, rustlog needs the chatroom id of the channel. The official API does not provide it, so it is looked up on Kick's website API (`kick.com/api/v2/channels/<slug>`) when a channel is joined for the first time:

1. A request from the built in HTTP client. Kick's website sits behind Cloudflare, which sometimes blocks clients whose TLS handshake does not look like a browser's.
2. If that fails, the request is repeated with the `curl` binary (the Docker image includes it), whose handshake is usually let through.
3. If that fails too, the channel is tried again after a minute, then after longer and longer waits (doubling, up to once an hour). As a way out, the chatroom id can be put into the config (`chatroomIds`).

Found chatroom ids are stored in the `kick_chatrooms` table, so restarting does not repeat the lookups (which are rate limited and slow down to three per second).

To check whether the lookup works from your server, run this on it. A JSON document starting with `{"id":` means it does, an HTML page ("Just a moment...") means Cloudflare blocks the server, in which case use `chatroomIds`:

```
curl -s -A "Mozilla/5.0" https://kick.com/api/v2/channels/xqc | head -c 300
```

The value to put into `chatroomIds` is `chatroom.id` of that document, together with the `user_id` of the channel: `"chatroomIds": {"676": 668}`.

## What is logged

| Kick event | Stored as | Notes |
|------------|-----------|-------|
| chat message | `PRIVMSG` | emotes become Twitch style `emotes` tags |
| reply | `PRIVMSG` | with `reply-parent-msg-id`, `reply-parent-user-id`, `reply-parent-user-login`, `reply-parent-display-name`, `reply-parent-msg-body`, `reply-thread-parent-msg-id` |
| subscription celebration | `USERNOTICE` (`msg-id=resub`, or `sub` for a new subscription, or `celebration` for other kinds) | the message of the subscriber is the text |
| message deleted | `CLEARMSG` | `target-msg-id` points to the deleted message, which is still stored. `ai-moderated` and `violated-rules` if Kick's AI moderation deleted it, and the author (`target-user-id`, login, text) if the message was seen shortly before |
| ban / timeout | `CLEARCHAT` | `ban-duration` (seconds) and `ban-expires-at` for timeouts, `ban-permanent=1` for bans, `moderator-user-id` / `moderator-user-login` (who did it, if Kick says) |
| chat cleared | `CLEARCHAT` | without a target |
| unban | `USERNOTICE` (`msg-id=unban`) | `moderator-user-id` / `moderator-user-login` (who lifted it), `ban-permanent=1` if it was a permanent ban |
| subscription, gifted subscriptions | `USERNOTICE` (`sub`, `resub`, `subgift`, `submysterygift`) | |
| host / raid | `USERNOTICE` (`msg-id=raid`) | |
| channel point reward | `USERNOTICE` (`msg-id=reward-redeemed`) | |
| KICKs gifted | `USERNOTICE` (`msg-id=kicks-gifted`) | |

Messages are converted into the same model rustlog uses for Twitch, so the text, JSON, raw and ndjson formats work unchanged. The `raw` format is an IRC line rebuilt from the stored data (`:user!user@user.kick.com PRIVMSG #channel :text`), it exists for compatibility with tools that read IRC.

Mapping of the data:

- `badges` are `type/count`, for example `subscriber/3` (months) or `moderator/1`. Kick's cosmetic `badges_v2` are not stored.
- `subscriber`, `vip` and `mod` flags come from the badges. Kick does not send first-message, returning-chatter or room mode information, so the other flags are never set.
- Emotes: Kick sends `[emote:37226:KEKW]` inside the text. Rustlog stores the name (`KEKW`) in the text and the position in the `emotes` tag (`37226:6-9`), which is exactly what Twitch does, so the web interface can render them from `https://files.kick.com/emotes/<id>/fullsize`.
- `color` is the username color, `display-name` the username.

## Moderation data

Kick's chat publishes more about moderation than Twitch's IRC does, and rustlog-kick keeps all of it (see [API.md](./API.md#moderation) for the endpoints that answer questions about it):

| Event | What Kick sends | Kept as |
|-------|-----------------|---------|
| `UserBannedEvent` | the user, **who banned or timed them out**, whether it is permanent, the length in minutes and when it expires | `CLEARCHAT` with `moderator-user-id`, `moderator-user-login`, `ban-duration`, `ban-expires-at`, `ban-permanent` |
| `UserUnbannedEvent` | the user, **who lifted it**, whether the ban was permanent | `USERNOTICE` (`unban`) with `moderator-user-id`, `moderator-user-login`, `ban-permanent` |
| `MessageDeletedEvent` | the id of the message, whether the **AI moderation** deleted it and which **rules** it violated | `CLEARMSG` with `ai-moderated`, `violated-rules`; the author and the text are added from the messages seen shortly before |
| `ChatroomClearEvent` | nothing but the fact | `CLEARCHAT` without a target |

All of these also go into the table `moderation_actions`, which the moderation endpoints read. It is kept up to date by a materialized view on `message_structured` and built from the existing messages when the migration runs, like the rollups of the [statistics](#statistics-endpoints). Stop all instances of rustlog which use the database before upgrading to the first version which has it (and the rollups): they are filled from the existing messages, which is only correct if nothing writes messages in the meantime.

Some details:

- For some actions Kick sends the moderator with the id `0` and the name `moderator` (seen for timeouts). That is not a user, so no moderator is recorded then.
- Kick does not say who deletes a message, and does not publish the reason a moderator gave for a ban.
- Rustlog remembers the last 200 000 chat messages in memory, so that it can tell who wrote a message which is deleted a little later. Older messages are not found, those deletions stay without an author.
- If a user opted out, nothing about them is logged: neither their messages, nor bans of them, nor their deleted messages.

### Where else the data could come from

Kick's other interfaces were checked for more moderation data. None of them can be used to log arbitrary channels:

- **The official API** (REST, OAuth 2.1, `api.kick.com`) can ban, time out and unban users, but not read bans, and it needs the token of a moderator of the channel. There is no GraphQL API: both the official API and Kick's website API are REST.
- **The official webhook** `moderation.banned` also carries the reason of the ban. Webhooks need a public HTTPS endpoint and a subscription per channel (with limits for unverified apps), and the reason is not part of the chat events rustlog listens to. Rustlog does not use webhooks.
- **Kick's website API** lists the banned users of a channel, but only to its moderators (it needs a logged in session).

The app credentials (`clientID` and `clientSecret`) are needed for the same reason as in the Twitch version: to turn names into ids, not for the logging itself, which needs no account.

## Timestamps

Kick reports when a message was created with a precision of one second. Messages written in the same second would come back in arbitrary order, so rustlog uses the arrival time instead, limited to the second Kick reported, and makes sure that the timestamps of a channel only ever increase. The ordering within a second is therefore the order the messages arrived in.

Messages sent while rustlog is disconnected from Kick are lost, as there is no way to replay them.

## Scaling

### Join limits

The join limits are the same as in the original rustlog, whose IRC library (`twitch-irc`, `ClientConfig::new_simple`) uses these values. The websocket connections are managed the same way:

| Limit | Default | Option |
|-------|---------|--------|
| channels per connection | 90 | `pusherMaxChannelsPerConnection` |
| number of connections | not limited | `pusherMaxConnections` |
| time between opening two connections | a new connection is started at most every 2 seconds, one at a time | `pusherNewConnectionEveryMs` |

A new connection is opened when all existing ones carry as many channels as allowed. A connection is closed when its last channel is left, and a connection which was lost is opened again (which counts as opening a connection, so the pacing applies to reconnects too). Connections are opened one at a time: a connection keeps its turn until it is established, and the next one is started the configured time after that. If connecting fails, the next one may start right away.

The pacing means that a large number of channels takes a while to join: 90 channels per 2 seconds, so about 2 minutes for 5 000 channels and about 37 minutes for 100 000. Chat is logged from the moment a connection is up, there is no need to wait for the others.

The options exist so that the limits can be changed later without touching the code. For example, more channels per connection (`pusherMaxChannelsPerConnection`) need fewer connections and therefore fewer sockets, and `pusherNewConnectionEveryMs` makes large numbers of connections come up faster. Those would be changes to the defaults of the original rustlog, nothing of this is the default.

### Practical notes

- Every connection needs a socket. Rustlog raises the limit of open files on Linux as far as the system allows, and warns at startup if it is too low for the connections the configured channels need. In Docker the limit can be set with `--ulimit nofile=65536:65536`, as in the example in the README.
- The first start needs one website lookup per channel (about three per second, so 10 000 channels take an hour). Later starts read the stored chatroom ids.
- Resolving slugs and ids uses the official API in batches of 50 and is repeated hourly, like in the Twitch version.

## Statistics endpoints

The leaderboards, ranks, channel activity and user channel lists (see [API.md](./API.md#statistics)) read two rollup tables which are filled by materialized views whenever messages are written (the activity of a user in a channel and the first and last messages come from the messages themselves):

- `message_counts_daily`: chat messages per channel, user and day,
- `user_channel_stats`: first message, last message and message count per user and channel.

When rustlog starts on a database which already has messages, the rollups are built from them once (one partition at a time), which can take a while on large databases. They only count chat messages (`PRIVMSG`), not bans or notices. All periods are calendar periods in UTC, so the ClickHouse server has to run in UTC (the default of the official image).

The rollups also contain the messages of users and channels which opted out (their logs are not deleted either, they are only not served). The statistics queries leave them out where they can (the totals of `/channels/top` still include the messages of users who opted out). As with the rollups, stop all instances of rustlog which use the database before upgrading to the first version which has them.

## Known limitations

- Kick's websocket and website API are unofficial and can change. Rustlog skips events it cannot parse instead of failing.
- The official API may not know every account (for example deleted ones). Rustlog then falls back to the names it has logged.
- Only the events in the table above are logged. Polls, predictions, pinned messages and mode changes are ignored.
- Moderation events from before rustlog joined a chat, or while it was disconnected, are not available. A ban which was given while rustlog was away can be lifted again without rustlog knowing, so a ban in the history can be shown as going on while it is not.
