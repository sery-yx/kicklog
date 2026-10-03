# kicklog

> [!WARNING]
> **Most of this repository is vibecoded.** The code, tests and documentation were largely written with an AI coding assistant. It has unit tests and builds, but it has not been audited or run in production at scale. Review the code before relying on it, and do not expose the admin API to the internet without doing so.

A chat logging service for [Kick](https://kick.com), ported from [boring-nick/rustlog](https://github.com/boring-nick/rustlog) with the [improvements](#fork-improvements) of [ByteZ1337/rustlog](https://github.com/ByteZ1337/rustlog). It keeps rustlog's HTTP API, web interface, ClickHouse schema and chat commands, so existing tooling carries over.

Chat is stored in [ClickHouse](https://clickhouse.com) instead of text files, which keeps logs small and queries fast, even for very large channels.

## Features

- **Anonymous logging.** Any public Kick channel can be logged without an account in the chat. A Kick app is only used to look up names and ids.
- **Complete chat history.** Messages, replies, subscriptions and gifts, raids, channel point rewards, bans, timeouts and deleted messages.
- **Moderation history.** Who banned or timed out a user, for how long, and whether and by whom it was lifted, plus the rules Kick's AI moderation cited for a deleted message. See the [moderation endpoints](./docs/API.md#moderation).
- **Statistics.** Leaderboards, channel activity over time, the channels a user chats in, and first and last messages. See the [statistics endpoints](./docs/API.md#statistics).
- **Compatible API and UI.** The routes and response formats of rustlog (text, JSON, ndjson, raw), its web interface, and an OpenAPI reference at `/docs`.
- **Firehose.** A websocket at `/firehose` that streams every logged message as it arrives. See [the firehose](./docs/API.md#firehose).
- **Settings in the database.** Logged channels and opt-outs are stored in ClickHouse, so the config file is never rewritten.

How chat reaches the logger, and where Kick differs from Twitch, is described in [docs/KICK.md](./docs/KICK.md).

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

Set `clickhouseUrl` to `http://clickhouse:8123` in the config, then run `docker compose up -d`. To build the image yourself, use `docker build -t kicklog .`.

### From source

Requirements: a current stable Rust toolchain, [yarn](https://yarnpkg.com), and `curl` at runtime (a fallback for looking up chatrooms, see [docs/KICK.md](./docs/KICK.md#chatroom-ids)).

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

The web interface is then available at http://localhost:8025. The release binary is `target/release/rustlog-kick`.

## Usage

Channels are given as slugs (`xqc`) or Kick user ids. The `channels` in the config are imported into the database on the first start. After that, manage them through the admin API:

```
curl -X POST http://localhost:8025/admin/channels \
  -H "X-Api-Key: verysecurekey" -H "Content-Type: application/json" \
  -d '{"channels": ["xqc"]}'
```

Users listed in `admins` can do the same from chat with `!rustlog join <channel>` and `!rustlog leave <channel>`.

A few example requests:

```
GET /channel/xqc/user/some-user?reverse&limit=50     recent messages of a user
GET /channel/xqc/top?period=week                     most active chatters this week
GET /channel/xqc/user/some-user/bans                 bans and timeouts of a user
GET /user/some-user/channels                         the channels a user chats in
```

The full list is in [docs/API.md](./docs/API.md).

## Fork improvements

[ByteZ1337/rustlog](https://github.com/ByteZ1337/rustlog) is a Twitch fork of [boring-nick/rustlog](https://github.com/boring-nick/rustlog). Its changes were adapted for Kick as follows.

**Taken over**

- **Firehose websocket**, as IRC lines or as JSON with `?jsonBasic`, with `channel` added to the basic JSON, a `firehose` capability and a connected-clients metric.
- **UTC-independent queries.** Days are worked out in UTC, so ClickHouse does not need to run in UTC. This also covers the statistics added by this port.
- **Channels and opt-outs stored in the database** instead of the config file. Migration `12_user_tables` imports `channels` and `optOut` from an existing config once and removes them from the file.
- **400 channels per connection** by default (rustlog uses 90), configurable with `pusherMaxChannelsPerConnection`. Kick's own limits are not documented, so lower it if subscriptions are refused.
- **`/admin/check-users`** and the **`X-Opt-Out: true`** response header.

**Not taken over**

- **Stream logging.** It relies on Twitch's API.
- **The fork's deployment policies**: authentication on every route, opt-out disabled, and `/docs` and `/metrics` hidden. They would change how this service behaves.

Windows support and a bundled frontend were already present here.

## Documentation

| Document | Contents |
|----------|----------|
| [docs/API.md](./docs/API.md) | HTTP API: logs, statistics, moderation, firehose, admin, opting out |
| [docs/CONFIG.md](./docs/CONFIG.md) | Every config option and how to create a Kick app |
| [docs/KICK.md](./docs/KICK.md) | How chat is collected, what is stored, scaling, known limitations |
| [docs/MIGRATION.md](./docs/MIGRATION.md) | Importing existing raw logs |

## Development

Requirements: Rust, yarn, and Docker for a local ClickHouse (or your own installation).

```
cargo test                       # unit tests, no database needed
cargo clippy --all-targets
cd web && yarn build             # frontend, embedded into the binary from web/dist
```

The frontend is compiled into the binary, so run `yarn build` before `cargo build`, and again after changing it. For frontend work, `yarn start` in `web/` serves it with hot reload against a backend on port 8025.

## Status

This is a young port. Kick's chat websocket is unofficial and may change without notice, and moderation events that happen while the logger is offline cannot be recovered. See [Known limitations](./docs/KICK.md#known-limitations).

Bug reports and pull requests are welcome.

## Credits and license

Based on [boring-nick/rustlog](https://github.com/boring-nick/rustlog) and its web interface, which comes from a fork of [gempir/justlog](https://github.com/gempir/justlog), with improvements from [ByteZ1337/rustlog](https://github.com/ByteZ1337/rustlog). All are MIT licensed, and so is this project. See [LICENSE](./LICENSE), which keeps the original copyright notice.

Not affiliated with or endorsed by Kick.
