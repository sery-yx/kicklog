# Rustlog for Kick

## Description
Rustlog-kick is a [Kick](https://kick.com) chat logging service. It is a port of [rustlog](https://github.com/boring-nick/rustlog), the Twitch logger based on [justlog](https://github.com/gempir/justlog), and provides the same web UI and HTTP API, but with Kick channels and chats. Like the original it uses [Clickhouse](https://clickhouse.com) for storage instead of text files.

- Logs the chat of any Kick channel, no access to the channel needed and no account joins the chat (an app of a Kick account is only used to look up names and ids)
- Messages, replies, subscriptions and gifts, hosts, bans and timeouts, deleted messages
- Moderation as Kick publishes it: who banned or timed out a user, whether and when they were unbanned and by whom, which rules the AI moderation found deleted messages to violate, with [endpoints](./docs/API.md#moderation) for the history of a user and the actions of a channel
- Same HTTP API and web interface as the Twitch version, plus [statistics endpoints](./docs/API.md#statistics): leaderboards by day, week, month and year, activity over time, the channels a user chats in, first and last messages
- The same join limits as the original rustlog by default (90 channels per connection, no limit on connections, a new connection every 2 seconds), all of them adjustable in the config, see [KICK.md](./docs/KICK.md#scaling)

How it works and what differs from Twitch is described in [docs/KICK.md](./docs/KICK.md).

## Installation

Create a `config.json` file (see [CONFIG.md](./docs/CONFIG.md)). You need a Kick app for the `clientID` and `clientSecret`, [CONFIG.md](./docs/CONFIG.md#creating-a-kick-app) explains how to create one.

### Docker
```yaml
version: "3.8"
  
services:
  clickhouse:
    image: clickhouse/clickhouse-server:latest
    container_name: clickhouse
    volumes:
      - "./ch-data:/var/lib/clickhouse:rw"
    environment:
      CLICKHOUSE_DB: "rustlog"
      CLICKHOUSE_USER: "user"
      CLICKHOUSE_PASSWORD: "SuperSecretPassword"
    restart: unless-stopped
        
  rustlog:
    # the tag is the name of the branch the image was built from (`main` or `master`)
    image: ghcr.io/<your-account>/<your-repository>:main
    container_name: rustlog
    ports:
      - 8025:8025 
    volumes:
      - "./config.json:/config.json"
    # Every connection to Kick's chat needs a socket, see the scaling notes
    ulimits:
      nofile:
        soft: 65536
        hard: 65536
    depends_on: 
      - clickhouse
    restart: unless-stopped
```
The image is built by the GitHub workflow of this repository, or locally with `docker build -t rustlog-kick .`.

### From source

- Follow the [Contributing](#contributing) excluding the last step
- `cargo build --release`
- The resulting binary will be at `target/release/rustlog-kick`

`curl` should be installed (it is a fallback for looking up chatrooms, see [KICK.md](./docs/KICK.md#chatroom-ids)).

## Usage

Start logging channels by putting their slugs or user ids into `channels` in the config, or at runtime with the admin API:

```
curl -X POST http://localhost:8025/admin/channels \
  -H "X-Api-Key: verysecurekey" -H "Content-Type: application/json" \
  -d '{"channels": ["xqc"]}'
```

Admins (the `admins` list in the config) can do the same from chat with `!rustlog join <channel>` and `!rustlog leave <channel>`.

Then open http://localhost:8025 for the web interface, or use the [API](./docs/API.md):

```
GET /channel/xqc/user/some-user?reverse&limit=50     recent messages of a user
GET /channel/xqc/top?period=week                      most active chatters this week
GET /user/some-user/channels                          where a user chats
```

## Advantages over justlog

- Significantly better storage efficiency (3x+ improvement) thanks to not duplicating log files, more efficient structure and better compression (using ZSTD in Clickhouse)
- Blazing fast log queries with response streaming
- Support for ndjson logs responses

## Contributing

Requirements:
- rust
- yarn
- docker with docker-compose (optional, will need to set up Clickhouse manually without it)

Steps:

1. Set up the database (Clickhouse):

This repository provides a docker-compose to quickly set up Clickhouse. You can use it with:
```
docker-compose -f docker-compose.dev.yml up -d
```
Alternatively, you can install Clickhouse manually using the [official guide](https://clickhouse.com/docs/en/install). The server has to run in UTC (the default of the official image).

2. Create a config file

Copy `config.dist.json` to `config.json` and configure your database and Kick credentials. If you installed Clickhouse with Docker, the default database configuration works.

3. Build the frontend:
```
cd web
yarn install
yarn build
cd ..
```
4. Build and run rustlog:
```
cargo run
```

You can now access rustlog at http://localhost:8025.

Run the tests with `cargo test`. They do not need a database.

## Importing logs
See [MIGRATION.md](./docs/MIGRATION.md)

## Credits and license

This project is a port of [boring-nick/rustlog](https://github.com/boring-nick/rustlog) and its web interface ([boring-nick/justlog](https://github.com/boring-nick/justlog), `frontend-only-new`), both MIT licensed. See [LICENSE](./LICENSE). It is not affiliated with Kick.
