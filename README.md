# kicklog

> [!WARNING]
> **Disclaimer: the majority of this repository is vibecoded.** Most of the code, tests and documentation were written with an AI coding assistant, directed by prompts rather than written and reviewed line by line. It has unit tests and has been built, but it has not been audited and it has not been run against a production ClickHouse or at scale. Read the code before you rely on it, and do not expose it or its admin API to the internet without reviewing it yourself.

> **Based on [rustlog](https://github.com/boring-nick/rustlog) by boring-nick, with the improvements of the [ByteZ1337 fork](https://github.com/ByteZ1337/rustlog).**
> This project is a port of that Twitch chat logger to Kick. The architecture, HTTP API, ClickHouse schema and web interface originate there, and the original deserves the credit. Visit https://github.com/boring-nick/rustlog for the Twitch version. What was taken over from the fork is listed under [Changes from the ByteZ1337 fork](#changes-from-the-bytez1337-fork).

A chat logging service for [Kick](https://kick.com). It keeps rustlog's HTTP API, web interface, ClickHouse schema and chat commands, so existing tooling and habits carry over.

Chat is stored in [ClickHouse](https://clickhouse.com) instead of text files, which keeps the logs small and the queries fast even for very large channels.

## Features

- **Anonymous logging.** Any public Kick channel can be logged. No account joins the chat and no channel access is needed. A Kick app is only used to look up names and ids.
- **Complete chat history.** Messages, replies, subscriptions and gifts, raids, channel point rewards, bans, timeouts and deleted messages.
- **Moderation history.** Who banned or timed out a user, when, for how long, whether and by whom it was lifted, and which rules Kick's AI moderation cited when it deleted a message. See the [moderation endpoints](./docs/API.md#moderation).
- **Statistics.** Leaderboards by day, week, month and year, channel activity over time, the channels a user chats in, first and last messages. See the [statistics endpoints](./docs/API.md#statistics).
- **Compatible API and UI.** The same routes and response formats as rustlog (text, JSON, ndjson, raw), the same web interface, and an OpenAPI reference served at `/docs`.
- **Firehose.** A websocket at `/firehose` which sends every message of every logged channel as it arrives, see [the firehose](./docs/API.md#firehose).
- **Settings in the database.** The logged channels and the users who opted out are stored in ClickHouse, so the config file is never rewritten.
- **The fork's join limits by default.** 400 channels per connection (rustlog has 90), no cap on connections, one new connection every two seconds. All three are configurable, see [Scaling](./docs/KICK.md#scaling).

How chat reaches the logger and where Kick differs from Twitch is covered in [docs/KICK.md](./docs/KICK.md).

## Changes from the ByteZ1337 fork

[ByteZ1337/rustlog](https://github.com/ByteZ1337/rustlog) is a fork of rustlog for Twitch with improvements of its own, on its default branch `db-channels` and on its older `master` branch. This table lists every one of them and what became of it here:

| Change in the fork | Status in kicklog |
|--------------------|-------------------|
| **Firehose websocket** (`/firehose`): every logged message as an IRC line | Taken over. Opted-out users are not part of it, and the websockets are closed with a close frame when the server shuts down. |
| **Basic JSON firehose** (`/firehose?jsonBasic`) | Taken over. |
| **`channel` in the basic JSON** (`jsonBasic`, `ndjson`), no longer only in `json` | Taken over. |
| **Firehose client metric** `rustlog_firehose_clients_count`, and the fix of its count | Taken over, together with a `firehose` entry in `/capabilities`. |
| **Independent of the ClickHouse time zone**: the days with logs are worked out in UTC | Taken over. It is also applied to the statistics added by this port (daily rollup, activity of a user), so ClickHouse does not have to run in UTC anymore. |
| **Channels and opt-outs stored in the database** (tables `channel` and `opt_out`) instead of the config file | Taken over. Channels are Kick user ids or slugs. The migration `12_user_tables` imports `channels` and `optOut` of an older config once, and removes them from the file. |
| **Config without `channels` is accepted** (needed to run the migration without them) | Taken over. |
| **400 channels per connection** instead of 90 | Taken over: it is the default of `pusherMaxChannelsPerConnection`. Pusher does not limit it, Kick's own limits are not documented, lower it if subscriptions are refused. |
| **`/admin/check-users`**: which users have logs in a channel (`master`) | Taken over, for Kick user ids. |
| **`X-Opt-Out: true` header** on responses for opted-out channels and users (`master`) | Taken over. |
| **Windows support** (`master`) | Already there: shutdown works on Windows, and this port builds and passes its tests on Windows. |
| **Frontend inside the repository** (`master`) | Already there: it is in `web/`. |
| **Name history improvements** (`master`) | Not separate: this port has rustlog's name history built on a materialized view, which the fork merged as well. |
| **Stream logging** (`master`): the streams of all Twitch channels from the Twitch API, and an admin route for them | Not taken over. It is specific to Twitch's API. |
| **Auth key for every route, opt-out disabled, full channel logs disabled, `/docs` and `/metrics` hidden, a user exempt from auth** (`master`) | Not taken over. These are policy decisions of the fork's own deployment, not improvements, and they would change how this service behaves. Opting out works as in rustlog, and the routes are public except `/admin`. |

The other settings of the fork's Twitch IRC client (message queue size, delay between messages, connect timeout) have no equivalent here: this logger only listens, it never sends chat messages.

## Getting started

You need a Kick app for the `clientID` and `clientSecret` in the config. [docs/CONFIG.md](./docs/CONFIG.md#creating-a-kick-app) explains how to create one and lists every option.

### Docker

Create a `config.json` next to a `docker-compose.yml`:

```yaml
services:
  clickhouse:
    image: clickhouse/clickhouse-server:24.3
    volumes:
      - ./ch-data:/var/lib/clickhouse
    environment:
      CLICKHOUSE_DB: rustlog
      CLICKHOUSE_USER: user
      CLICKHOUSE_PASSWORD: SuperSecretPassword
    restart: unless-stopped

  kicklog:
    image: ghcr.io/sery-yx/kicklog:main
    ports:
      - 8025:8025
    volumes:
      - ./config.json:/config.json
    # every websocket connection to Kick needs a socket
    ulimits:
      nofile:
        soft: 65536
        hard: 65536
    depends_on:
      - clickhouse
    restart: unless-stopped
```

Set `clickhouseUrl` to `http://clickhouse:8123` in the config, then run `docker compose up -d`.

To build the image yourself, use `docker build -t kicklog .`.

### From source

Requirements: a current stable Rust toolchain, [yarn](https://yarnpkg.com), and `curl` at runtime (it is a fallback for looking up chatrooms, see [KICK.md](./docs/KICK.md#chatroom-ids)).

```
git clone https://github.com/sery-yx/kicklog.git
cd kicklog

# ClickHouse for development
docker compose -f docker-compose.dev.yml up -d

cp config.dist.json config.json    # then add your Kick credentials

# web interface
cd web && yarn install && yarn build && cd ..

cargo run --release
```

The web interface is then available at http://localhost:8025. A release binary is written to `target/release/rustlog-kick`.

## Usage

Channels are given as slugs (`xqc`) or Kick user ids. The channels in `channels` in the config are imported into the database the first time the logger starts, after that they are managed through the admin API (they are stored in the database, and removed from the config file):

```
curl -X POST http://localhost:8025/admin/channels \
  -H "X-Api-Key: verysecurekey" -H "Content-Type: application/json" \
  -d '{"channels": ["xqc"]}'
```

Users listed in `admins` can do the same from chat with `!rustlog join <channel>` and `!rustlog leave <channel>`.

A few requests to get a feel for the API:

```
GET /channel/xqc/user/some-user?reverse&limit=50     recent messages of a user
GET /channel/xqc/top?period=week                     most active chatters this week
GET /channel/xqc/user/some-user/bans                 bans and timeouts of a user
GET /user/some-user/channels                         the channels a user chats in
```

The full list is in [docs/API.md](./docs/API.md).

## Documentation

| Document | Contents |
|----------|----------|
| [docs/API.md](./docs/API.md) | HTTP API: logs, statistics, moderation, admin, opting out |
| [docs/CONFIG.md](./docs/CONFIG.md) | Every config option and how to create a Kick app |
| [docs/KICK.md](./docs/KICK.md) | How chat is collected, what is stored, scaling, known limitations |
| [docs/MIGRATION.md](./docs/MIGRATION.md) | Importing existing raw logs |

## Development

Requirements: Rust, yarn, and Docker for a local ClickHouse (or a ClickHouse installation of your own).

```
cargo test                       # unit tests, no database needed
cargo clippy --all-targets
cd web && yarn build             # frontend, embedded into the binary from web/dist
```

The frontend is compiled into the binary, so run `yarn build` before `cargo build` and again after changing it. For frontend work, `yarn start` in `web/` serves it with hot reload against a backend running on port 8025.

## Status

This is a young, largely AI-written port (see the disclaimer at the top). Kick's chat websocket is unofficial and Kick may change it without notice, so expect to follow upstream changes now and then. Moderation events that happened while the logger was offline cannot be recovered. The remaining caveats are listed under [Known limitations](./docs/KICK.md#known-limitations).

Bug reports and pull requests are welcome.

## Credits and license

This project is based on [boring-nick/rustlog](https://github.com/boring-nick/rustlog), including its web interface, which comes from a fork of [gempir/justlog](https://github.com/gempir/justlog). It also takes over improvements from [ByteZ1337/rustlog](https://github.com/ByteZ1337/rustlog), a fork of rustlog (see [Changes from the ByteZ1337 fork](#changes-from-the-bytez1337-fork)). All of them are MIT licensed. This project is released under the same license, see [LICENSE](./LICENSE), which keeps the original copyright notice.

Not affiliated with or endorsed by Kick.
