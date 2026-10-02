# kicklog

> **Based on [rustlog](https://github.com/boring-nick/rustlog) by boring-nick.**
> This project is a port of that Twitch chat logger to Kick. The architecture, HTTP API, ClickHouse schema and web interface originate there, and the original deserves the credit. Visit https://github.com/boring-nick/rustlog for the Twitch version.

A chat logging service for [Kick](https://kick.com). It keeps rustlog's HTTP API, web interface, ClickHouse schema and chat commands, so existing tooling and habits carry over.

Chat is stored in [ClickHouse](https://clickhouse.com) instead of text files, which keeps the logs small and the queries fast even for very large channels.

## Features

- **Anonymous logging.** Any public Kick channel can be logged. No account joins the chat and no channel access is needed. A Kick app is only used to look up names and ids.
- **Complete chat history.** Messages, replies, subscriptions and gifts, raids, channel point rewards, bans, timeouts and deleted messages.
- **Moderation history.** Who banned or timed out a user, when, for how long, whether and by whom it was lifted, and which rules Kick's AI moderation cited when it deleted a message. See the [moderation endpoints](./docs/API.md#moderation).
- **Statistics.** Leaderboards by day, week, month and year, channel activity over time, the channels a user chats in, first and last messages. See the [statistics endpoints](./docs/API.md#statistics).
- **Compatible API and UI.** The same routes and response formats as rustlog (text, JSON, ndjson, raw), the same web interface, and an OpenAPI reference served at `/docs`.
- **Rustlog's join limits by default.** 90 channels per connection, no cap on connections, one new connection every two seconds. All three are configurable, see [Scaling](./docs/KICK.md#scaling).

How chat reaches the logger and where Kick differs from Twitch is covered in [docs/KICK.md](./docs/KICK.md).

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

# ClickHouse for development; the server must run in UTC (the default of the official image)
docker compose -f docker-compose.dev.yml up -d

cp config.dist.json config.json    # then add your Kick credentials

# web interface
cd web && yarn install && yarn build && cd ..

cargo run --release
```

The web interface is then available at http://localhost:8025. A release binary is written to `target/release/rustlog-kick`.

## Usage

Channels are given as slugs (`xqc`) or Kick user ids. List them in `channels` in the config, or add them at runtime through the admin API:

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

This is a young port. Kick's chat websocket is unofficial and Kick may change it without notice, so expect to follow upstream changes now and then. Moderation events that happened while the logger was offline cannot be recovered. The remaining caveats are listed under [Known limitations](./docs/KICK.md#known-limitations).

Bug reports and pull requests are welcome.

## Credits and license

This project is based on [boring-nick/rustlog](https://github.com/boring-nick/rustlog), including its web interface, which comes from a fork of [gempir/justlog](https://github.com/gempir/justlog). Both are MIT licensed. This project is released under the same license, see [LICENSE](./LICENSE), which keeps the original copyright notice.

Not affiliated with or endorsed by Kick.
