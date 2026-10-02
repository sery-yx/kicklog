# HTTP API

The API of rustlog-kick is the API of rustlog (and justlog) with Kick ids, plus the [statistics](#statistics) and [moderation](#moderation) endpoints described below. An interactive reference of every endpoint is served at `/docs` and the OpenAPI document at `/openapi.json`. `/capabilities` (and the `x-rustlog-capabilities` response header) lists which optional features the server supports.

## Names and ids

Channels and users can be given by name or by id. Names are Kick slugs (`some-user`, usernames like `Some_User` are accepted too), ids are Kick user ids:

| Path segment | Meaning |
|--------------|---------|
| `/channel/<slug>` | a channel by name |
| `/channelid/<id>` | a channel by user id |
| `/user/<slug>` | a user by name |
| `/userid/<id>` | a user by user id |

`/channel/xqc/user/some-user` and `/channelid/676/userid/12345` are equivalent forms.

## Logs

| Endpoint | Description |
|----------|-------------|
| `GET /channels` | The logged channels, as `{"channels": [{"name": "xqc", "userID": "676"}]}` |
| `GET /list?channel=<slug>` or `?channelid=<id>`, optionally `&user=<slug>` or `&userid=<id>` | The days (for a channel) or months (for a user) which have logs |
| `GET /channel/<channel>` | Redirects to the most recent day with logs |
| `GET /channel/<channel>/<year>/<month>/<day>` | The logs of a channel for a day |
| `GET /channel/<channel>/user/<user>` | Redirects to the most recent month with logs of the user |
| `GET /channel/<channel>/user/<user>/<year>/<month>` | The logs of a user in a channel for a month |
| `GET /channel/<channel>?from=<time>&to=<time>` | The logs of a channel for a range (RFC 3339 times), also available for users |
| `GET /channel/<channel>/random` | A random message |
| `GET /channel/<channel>/user/<user>/random` | A random message of a user |
| `GET /channel/<channel>/user/<user>/search?q=<text>` | The messages of a user which contain the text |
| `GET /channel/<channel>/stats` | Number of messages and the top 5 chatters, optionally with `from` and `to` |
| `GET /channel/<channel>/user/<user>/stats` | Number of messages of a user, optionally with `from` and `to` |
| `GET /namehistory/<user id>` | The names a user has used |

### Response formats

Every endpoint which returns messages understands the same parameters (their presence is enough, the value is ignored):

| Parameter | Effect |
|-----------|--------|
| *(none)* | Plain text, one line per message: `[2026-10-02 06:38:02] #channel user: text` |
| `json` | `{"messages": [...]}` with all details: `text`, `displayName`, `timestamp`, `id`, `tags`, `username`, `channel`, `raw` and `type` |
| `jsonBasic` | The same without `username`, `channel`, `raw` and `type` |
| `ndjson` | One JSON object (as in `jsonBasic`) per line |
| `raw` | IRC style lines rebuilt from the stored data |
| `reverse` | Newest messages first |
| `limit=<n>`, `offset=<n>` | Paging |

`type` is rustlog's numeric message type: `1` chat message, `2` ban/timeout/chat clear, `4` notice (subscriptions, gifts, hosts, unbans, ...), `13` deleted message. See [KICK.md](./KICK.md#what-is-logged) for how Kick events are mapped.

The lines of bans and timeouts say who did it (`timed out for 600 seconds by some-mod`), and the tags of moderation lines have more details than on Twitch, see [Moderation](#moderation).

## Statistics

The statistics endpoints only count chat messages. Leaderboards, ranks, channel activity and the channels of a user are answered from precomputed totals, and everything about a user in a channel uses the primary key of the message table, so they stay fast for large channels. Periods are calendar periods in **UTC**, weeks start on Monday.

Results which include the current day are cached for a minute, finished periods for ten hours.

### Leaderboards

`GET /channel/<channel>/top` returns the chatters with the most messages in a period.

| Parameter | Description |
|-----------|-------------|
| `period` | `day` (default), `week`, `month`, `year` or `all` |
| `date` | Any date inside the period: `2026-09-15`, `2026-09` or `2026`. Defaults to today. Ignored for `all`. |
| `limit` | 1 to 100, default 10 |

```
GET /channel/xqc/top?period=week
GET /channel/xqc/top?period=month&date=2026-09&limit=25
GET /channelid/676/top?period=year&date=2025
```

```json
{
  "period": "week",
  "from": "2026-09-28T00:00:00Z",
  "to": "2026-10-05T00:00:00Z",
  "messageCount": 48210,
  "topChatters": [
    {"userId": "12345", "userLogin": "some-user", "messageCount": 912}
  ]
}
```

`GET /channels/top` ranks the channels by their number of messages, with the same parameters:

```json
{
  "period": "day",
  "from": "2026-10-02T00:00:00Z",
  "to": "2026-10-03T00:00:00Z",
  "channels": [
    {"channelId": "676", "channelLogin": "xqc", "messageCount": 80211}
  ]
}
```

`/channel/<channel>/top` leaves users who opted out out of the list and of `messageCount`. `/channels/top` leaves out channels which opted out, its counts still include the messages of users who opted out. With `period=all`, `from` and `to` are not in the responses.

### Rank of a user

`GET /channel/<channel>/user/<user>/rank?period=month&date=2026-09` returns how many messages a user wrote in the period and their position among the chatters of the channel (users with the same count share a rank).

```json
{
  "userId": "12345",
  "userLogin": "some-user",
  "period": "month",
  "from": "2026-09-01T00:00:00Z",
  "to": "2026-10-01T00:00:00Z",
  "messageCount": 912,
  "rank": 7,
  "totalChatters": 3120
}
```

`rank` is missing when the user did not write anything in the period.

### Activity over time

`GET /channel/<channel>/activity` returns the messages and the number of different chatters per bucket, and `GET /channel/<channel>/user/<user>/activity` the messages of a user (without chatter counts). Buckets without messages are included with a count of zero.

| Parameter | Description |
|-----------|-------------|
| `interval` | The bucket size: `day` (default), `week`, `month` or `year` |
| `from` | First day, `2026-09-01` (an RFC 3339 time is reduced to its UTC date). Default: 30 days, 182 days (26 weeks), 365 days or 1825 days up to and including `to`, depending on the interval, before the range is widened (so the current month or year is one more bucket) |
| `to` | Last day (inclusive). Default: today |

The range is widened to whole buckets, so that no bucket counts only some of its days: it starts with the interval `from` is in and ends with the interval `to` is in (weeks start on Monday). It may cover at most 3660 days after that. `from` and `to` of the response are the start and the end (exclusive) of the widened range, the end can be in the future.

```
GET /channel/xqc/activity?interval=week&from=2026-07-01
```

```json
{
  "interval": "week",
  "from": "2026-06-29T00:00:00Z",
  "to": "2026-10-05T00:00:00Z",
  "buckets": [
    {"start": "2026-06-29T00:00:00Z", "messageCount": 301442, "uniqueChatters": 21040}
  ]
}
```

Users who opted out are not counted in the activity of a channel.

### Where a user chats

`GET /user/<user>/channels` (or `/userid/<id>/channels`) lists the channels a user has chatted in, with the number of messages and the time of the first and the last one.

| Parameter | Description |
|-----------|-------------|
| `sort` | `last` (default, most recently active channel first) or `count` (most messages first) |
| `limit` | 1 to 100, default 25 |
| `offset` | Number of channels to skip, default 0 |

```json
{
  "userId": "12345",
  "userLogin": "some-user",
  "channels": [
    {
      "channelId": "676",
      "channelLogin": "xqc",
      "messageCount": 912,
      "firstMessage": "2026-01-14T16:08:06Z",
      "lastMessage": "2026-10-02T06:38:02.123Z"
    }
  ]
}
```

Channels which opted out are not listed.

`GET /user/<user>/summary` (or `/userid/<id>/summary`) adds up everything: the number of messages and channels, the first and the last message, and the channel of the most recent message.

```json
{
  "userId": "12345",
  "userLogin": "some-user",
  "messageCount": 4310,
  "channelCount": 14,
  "firstMessage": "2026-01-14T16:08:06Z",
  "lastMessage": "2026-10-02T06:38:02.123Z",
  "lastChannelId": "676",
  "lastChannelLogin": "xqc"
}
```

### First and last message

These return a single message in the [formats](#response-formats) above (plain text by default):

| Endpoint | Message |
|----------|---------|
| `GET /channel/<channel>/user/<user>/first` | The first message of a user in a channel |
| `GET /channel/<channel>/user/<user>/last` | The most recent message of a user in a channel |
| `GET /user/<user>/last` | The most recent message of a user in any channel (channels which opted out are skipped) |

## Moderation

Kick publishes more about moderation than Twitch does: who banned or timed out a user, who lifted it, and, when the AI moderation deleted a message, which rules the message violated. Rustlog-kick keeps all of it and answers questions about it:

| Endpoint | Description |
|----------|-------------|
| `GET /channel/<channel>/user/<user>/bans` | The bans, timeouts and unbans of a user in a channel, and how every ban and timeout ended |
| `GET /user/<user>/bans` | The same in all channels |
| `GET /channel/<channel>/moderation` | The moderation actions of a channel, the newest first |
| `GET /channel/<channel>/moderators` | The moderators of a channel with the most actions in a period |

Everything is as far as it was logged: what happened before rustlog joined a channel, or while it was disconnected, is not known. Kick does not publish the reason a moderator gave, and for some actions it does not say who did it (for example for some timeouts), the moderator is missing then. Moderators who opted out are not named. Users who opted out are left out of the lists of actions (in the ranking of moderators only moderators who opted out are left out).

### Bans and timeouts of a user

`GET /channel/<channel>/user/<user>/bans` answers who banned or timed out a user, when, whether the user was unbanned, when, and by whom. `GET /user/<user>/bans` does the same for all channels and lists the channels the user is banned in right now. Both support `order` (`desc`, the default, or `asc`), `limit` (1 to 500, default 100) and `offset`.

```
GET /channel/xqc/user/some-user/bans
```

```json
{
  "channelId": "676",
  "channelLogin": "xqc",
  "userId": "12345",
  "userLogin": "some-user",
  "banned": false,
  "bans": 0,
  "timeouts": 1,
  "unbans": 1,
  "total": 2,
  "truncated": false,
  "actions": [
    {
      "type": "unban",
      "timestamp": "2026-10-02T06:41:10.532Z",
      "channelId": "676",
      "channelLogin": "xqc",
      "user": {"id": "12345", "login": "some-user"},
      "moderator": {"id": "20", "login": "some-mod"},
      "permanent": false
    },
    {
      "type": "timeout",
      "timestamp": "2026-10-02T06:38:04.120Z",
      "channelId": "676",
      "channelLogin": "xqc",
      "user": {"id": "12345", "login": "some-user"},
      "moderator": {"id": "21", "login": "other-mod"},
      "durationSeconds": 600,
      "expiresAt": "2026-10-02T06:48:04Z",
      "end": {
        "at": "2026-10-02T06:41:10.532Z",
        "reason": "unban",
        "moderator": {"id": "20", "login": "some-mod"}
      }
    }
  ]
}
```

- `type` is `ban` (permanent), `timeout`, or `unban`. A timeout has `durationSeconds` and `expiresAt`.
- `end` tells how a ban or timeout ended, it is missing while it is still going on (`banned` is `true` then). `reason` is `unban` (a moderator lifted it, `moderator` is who), `expired` (the timeout ran out, `at` is when) or `superseded` (a newer ban or timeout took its place, `moderator` is who gave that one).
- `permanent` of an unban is what Kick says about the ban which was lifted: whether it was a permanent one and not a timeout.
- `bans`, `timeouts`, `unbans` and `total` count the whole history, not only the entries returned. At most the 2000 newest actions are read, `truncated` is `true` when there were more, then the oldest ones are missing.

`GET /user/<user>/bans` has `bannedIn` (the channels `{"id", "login"}` in which a ban or timeout is going on) instead of `banned` and `channelId`/`channelLogin`, and every entry has its channel. Channels which opted out are not listed.

### Moderation actions of a channel

`GET /channel/<channel>/moderation` lists everything moderators did in a channel, the newest first.

| Parameter | Description |
|-----------|-------------|
| `type` | Only this kind of action: `ban`, `timeout`, `unban`, `delete` (a message was deleted) or `clear` (the chat was cleared) |
| `moderator` | Only actions of the moderator with this login |
| `moderatorId` | Only actions of the moderator with this user id |
| `from`, `to` | Only actions in this range, RFC 3339 times or dates (the start of the day in UTC) |
| `limit` | 1 to 500, default 50 |
| `offset` | Number of actions to skip, default 0 |

```
GET /channel/xqc/moderation?type=delete&limit=2
```

```json
{
  "channelId": "676",
  "channelLogin": "xqc",
  "actions": [
    {
      "type": "delete",
      "timestamp": "2026-10-02T06:44:21.903Z",
      "channelId": "676",
      "channelLogin": "xqc",
      "user": {"id": "12345", "login": "some-user"},
      "messageId": "c52ca12d-3cd1-4471-80ed-2cf73bac96a1",
      "text": "the deleted message",
      "aiModerated": true,
      "violatedRules": ["sexual"]
    }
  ]
}
```

Deleted messages have `aiModerated` and `violatedRules` when Kick's AI moderation deleted them. `user` and `text` (who wrote the message and what it said) are only there if the message was seen in chat shortly before it was deleted (rustlog remembers the last 200 000 messages), `moderator` is never there for deleted messages as Kick does not say who deletes them.

### Moderators of a channel

`GET /channel/<channel>/moderators` ranks the moderators by how many bans, timeouts and unbans they did in a calendar period, with the parameters of the [leaderboards](#leaderboards) (`period`, `date`, `limit`).

```json
{
  "channelId": "676",
  "channelLogin": "xqc",
  "period": "month",
  "from": "2026-10-01T00:00:00Z",
  "to": "2026-11-01T00:00:00Z",
  "moderators": [
    {"moderator": {"id": "20", "login": "some-mod"}, "bans": 3, "timeouts": 41, "unbans": 2, "total": 46}
  ]
}
```

### In the logs

The moderation lines of the [log endpoints](#logs) carry the same data as tags (`json` and `raw` formats): `moderator-user-id` and `moderator-user-login` for bans, timeouts and unbans, `ban-duration` (seconds) and `ban-expires-at` for timeouts, `ban-permanent=1` for bans (and for unbans of permanent bans), `target-user-id` for bans and timeouts, and `target-msg-id`, `ai-moderated` and `violated-rules` (plus `target-user-id`, if the author is known) for deleted messages. The text of these lines says who did it: `some-user has been timed out for 600 seconds by some-mod`.

## Other endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /metrics` | Prometheus metrics |
| `GET /capabilities` | The optional features the server supports |
| `GET /docs`, `GET /openapi.json` | The interactive reference and the OpenAPI document |

## Admin

Requests to `/admin/...` need the configured `adminAPIKey` in the `X-Api-Key` header.

| Endpoint | Description |
|----------|-------------|
| `POST /admin/channels` | Starts logging channels. Body: `{"channels": ["xqc", "7183419"]}`, each entry a slug or a user id |
| `DELETE /admin/channels` | Stops logging channels, same body. Ids of channels which Kick does not know anymore (deleted accounts) are removed as well |

## Opting out

`POST /optout` returns a code which is valid for 60 seconds. Sending `!rustlog optout <code>` in a logged chat opts the sender out: their messages are no longer logged and their logs are no longer served. Admins can opt out other users with `!rustlog optout <name>`.
