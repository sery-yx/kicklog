# Configuration

Configuration is stored in a `config.json` file. The keys are the same as in the Twitch version of rustlog, `channels` and the chat related values refer to Kick instead.

Available options:
- `clickhouseUrl` (string): Connection URL for Clickhouse. Note that it should start with the protocol (`http://`)
- `clickhouseDb` (string): Clickhouse database name. Use a database of its own, Kick and Twitch ids must not be mixed.
- `clickhouseUsername` (string): Clickhouse username.
- `clickhousePassword` (string): Clickhouse password.
- `clickhouseFlushInterval` (number): Interval (in seconds) of how often messages should be flushed to the database. A lower value means that logs are available sooner at the expensive of higher database load. Defaults to 10.
- `listenAddress` (string): Listening address for the web server. Defaults to `0.0.0.0:8025`.
- `channels` (array of strings): The channels to be logged, each a **Kick user id** or a channel slug (`xqc`). The user id is not Kick's channel id or chatroom id, see [KICK.md](./KICK.md#finding-ids) for how to look it up. Optional, and only read by the first start of a version which stores the channels in the database, see [Channels and opt outs](#channels-and-opt-outs).
- `clientID` (string): Client id of your Kick app.
- `clientSecret` (string): Client secret of your Kick app.
- `admins` (array of strings): List of Kick usernames (or slugs) who are allowed to use administration commands.
- `adminAPIKey` (string): API key for admin requests. Optional: without it every request to `/admin` is refused.

## Channels and opt outs

The logged channels and the users who opted out are stored in the database (the tables `channel` and `opt_out`), not in the config. They change while the logger runs (the admin API, `!rustlog join`, opting out), and the config file is never written to for that.

`channels` and `optOut` of an older config (or a config written for the first start) are imported into the database once, by the migration `12_user_tables` when the logger starts, and are removed from the file when that is done. After that they are ignored: change the channels with the admin API (or `!rustlog join` and `!rustlog leave` in a chat) and opting out with `!rustlog optout`. If the file cannot be written, a warning is logged and the settings stay in it, ignored.

Kick specific options, all of them are optional:
- `pusherMaxChannelsPerConnection` (number): How many channels are listened to on one websocket connection. Defaults to 400, which is the value of the ByteZ1337 fork of rustlog (the original rustlog has 90).
- `pusherMaxConnections` (number): How many websocket connections are opened at most. Not limited if unset or 0 (the default), like in the original rustlog. Operating systems commonly limit a process to 1024 open sockets, see [KICK.md](./KICK.md#scaling) for how the limits work together.
- `pusherNewConnectionEveryMs` (number): Websocket connections are opened one at a time, and a new one is started at most this many milliseconds after the previous one was established. Defaults to 2000, like in the original rustlog.
- `chatroomIds` (object of strings: numbers): Chatroom ids to use instead of looking them up, as `"<user id of the channel>": <chatroom id>`. Only needed if Kick's website cannot be reached from your server, see [KICK.md](./KICK.md#chatroom-ids).
- `pusherKey` (string), `pusherCluster` (string): The credentials of the websocket Kick's web client uses. The defaults work, change them only if Kick changes them.

## Creating a Kick app

`clientID` and `clientSecret` belong to a Kick app. It is only used to turn channel names into user ids and back, rustlog never acts on behalf of a Kick account.

1. Sign in on [kick.com](https://kick.com) and open the [developer settings](https://kick.com/settings/developer) of your account (two-factor authentication has to be enabled for it).
2. Create an app. The redirect URL and the webhook settings are not used, enter any valid URL.
3. Copy the client id and the client secret into `config.json`.

Example config:
```json
{
  "clickhouseUrl": "http://clickhouse:8123",
  "clickhouseDb": "rustlog",
  "clickhouseUsername": "user",
  "clickhousePassword": "SuperSecretPassword",
  "listenAddress": "0.0.0.0:8025",
  "channels": ["676"],
  "clientID": "id",
  "clientSecret": "secret",
  "admins": [],
  "adminAPIKey": "verysecurekey"
}
```
