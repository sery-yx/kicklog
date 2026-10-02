# Importing raw logs

Rustlog-kick can import raw chat logs from a folder in justlog's layout into the database. This is meant for moving logs between rustlog instances (for example the output of the `?raw` parameter of the [API](./API.md#response-formats)) and for logs of other tools which can write IRC style lines. There is no justlog for Kick, so most installations never need this.

## Format

The folder contains one folder per **channel id** (the Kick user id of the channel), with the logs of every day in `<year>/<month>/<day>/channel.txt` (or `channel.txt.gz`):

```
logs/
  676/
    2026/
      10/
        2/
          channel.txt
```

Each line is an IRC message with tags, as returned by the `raw` format:

```
@tmi-sent-ts=1790923082123;room-id=676;user-id=12345;display-name=Some_User;badges=subscriber/3;color=#BC66FF;id=21d47bf1-1486-4228-b4c7-420fe65e40b0 :some-user!some-user@some-user.kick.com PRIVMSG #xqc :hello
```

The important tags are `tmi-sent-ts` (milliseconds since the epoch) and `user-id` (or `target-user-id`). The channel id is the name of the folder the log file is in. A line without `tmi-sent-ts` gets midnight of its day as the time. Lines which cannot be parsed are skipped and logged.

## Config

The importer uses the database settings of the config, see [CONFIG.md](./CONFIG.md) for the keys starting with `clickhouse`.

## Running the import

First, rustlog needs to have access to the logs directory. If using docker, you need to add it as a volume mount to the container.

Docker:
```
docker exec -it rustlog rustlog-kick migrate --source-dir /logs --jobs 1
```
Manual installation:
```
rustlog-kick migrate --source-dir /path/to/logs --jobs 1
```
The `--jobs` parameter defines how many threads rustlog will use for importing. If your logs are on a HDD, you should keep it at 1, as IO will likely be the bottleneck anyway. If you have an SSD, then setting the value to half of your CPU threads should generally work well. With `--channel-id` (can be repeated) only the given channels are imported.

The import can take anywhere from a few minutes to a few hours depending on your amount of logs and system resources. Imported messages also appear in the statistics endpoints, as those are filled by the database whenever messages are written.
