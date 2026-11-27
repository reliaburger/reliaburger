# Watching Everything

You can build the most reliable container orchestrator in the world, but if you can't see what it's doing, you're flying blind. Phase 6 adds observability: metrics collection, log capture, alerting, and a dashboard. All built in. No Prometheus server to deploy, no Elasticsearch cluster to manage, no Grafana to configure.

## Why not just use Prometheus?

Prometheus is excellent. It's the industry standard for metrics. But it's also a separate system: you deploy it, configure scraping targets, set up alerting rules, run Alertmanager for notifications, deploy Grafana for dashboards, and manage all their storage. That's four more services to keep running, each with its own failure modes.

Reliaburger takes a different approach. The metrics database, log collector, alert evaluator, and dashboard are compiled into the same `bun` binary that runs your containers. When the node starts, observability starts. When the node stops, it stops. No separate lifecycle to manage.

## Standing on the shoulders of InfluxDB

We could have written a custom time-series database from scratch. Gorilla XOR compression, WAL segments, compaction, the whole thing. It would have taken months and introduced subtle correctness bugs that take years to shake out.

Instead, we reuse the same building blocks that power InfluxDB 3.0, DeltaLake, and Apache Iceberg:

- **Arrow** for columnar in-memory storage
- **DataFusion** for SQL queries
- **Parquet** for on-disk persistence
- **object_store** for storage abstraction (local disk or S3)

This gives us a production-grade metrics engine in a few hundred lines of glue code. The heavy lifting — columnar compression, predicate pushdown, vectorised execution — is handled by libraries that thousands of engineers have battle-tested.

## SQL, not PromQL

Here's a controversial choice. Prometheus uses PromQL, a purpose-built query language for time-series data. It's powerful, but it confuses people. Even experienced engineers struggle with the difference between `rate()` and `irate()`, with range vectors versus instant vectors, with the `offset` modifier. PromQL is a language you have to learn, and most people learn just enough to copy-paste from Stack Overflow.

Our metrics use SQL:

```sql
SELECT timestamp, metric_name, value
FROM metrics
WHERE metric_name = 'node_cpu_usage_percent'
AND timestamp > 1704067200
ORDER BY timestamp
```

If you know SQL, you already know how to query our metrics. No new DSL to learn. DataFusion gives us the full SQL engine — aggregations, joins, subqueries, window functions — for free.

Could we add PromQL support later? Yes. A translator covering the 20% of PromQL that people actually use — `rate()`, `sum by()`, `avg by()`, `histogram_quantile()`, comparison operators — would let existing Grafana dashboards work without rewriting queries. That's a Phase 11 job.

## The storage abstraction

Here's where it gets interesting. The `object_store` crate abstracts over local filesystems, S3, GCS, and Azure Blob Storage. DataFusion reads Parquet files from any of them transparently.

On your dev laptop, metrics write to `~/.local/share/reliaburger/metrics/` as Parquet files. In production, you set one config field:

```toml
[metrics]
object_store_url = "s3://my-bucket/reliaburger-metrics"
```

Same code, same queries, same dashboard. The only difference is where the bytes go. Your metrics survive node failures because they're in S3, not on a local disk that just caught fire.

Remote metric chunks use random 128-bit names and create-only PUTs. Two nodes starting with the same empty bucket cannot choose `metrics_000000.parquet` and silently overwrite each other. Even a name collision refuses the second write and leaves its samples available for retry.

Bun opens a node-owned prefix beneath that bucket. For an enrolled node, the owner combines the cluster root CA fingerprint with the node ID in its certificate; renewing a leaf certificate under the same root and node ID keeps the same archive. A different root CA or a newly enrolled node identity gets a different prefix; the archive-wide reader can still inspect the earlier prefix. Plaintext nodes keep an opaque owner in `metrics-archive-owner` under the configured `[storage] data` directory. Back up that file: restoring only a remote bucket cannot identify which plaintext node's history to reopen. A human label or local directory path never chooses the prefix. Remote archive startup refuses an unwritable configured data directory or corrupt owner. Set `[storage] data` to a writable directory owned by that node, or restore its owner file from backup. Falling back to the common per-user state directory could give two nodes the same archive owner, so remote plaintext archives never use that fallback. Ordinary local metrics keep their existing fallback behaviour.

Each production node queries its own prefix, so cluster fan-out and rollups count each archive once. Remote retention still belongs to the bucket lifecycle policy; the existing remote prune path remains a no-op. `MayoStore::open` remains an archive-wide API for explicit offline queries; `open_for_node` is the production scope. This changes the durable ownership contract even though the Parquet columns stay the same, so it increments the state generation. Startup, joins and upgrades refuse the earlier generation; recreate the cluster with fresh node state rather than rolling across that boundary. The earlier shared prefix cannot prove which node owns each chunk, so we preserve it separately instead of guessing an owner or migrating it into a scoped archive.

### A flush needs an owner until acknowledgement

A remote PUT can store the object and then lose its reply. If the task that
awaited it disappears, did the samples disappear too? The request can't answer
that question. The store has to keep the batch.

The pending flush holds immutable encoded Parquet bytes, a publication identity
and a writer guard. Cancelling an async caller drops its handle, while a blocking
writer can still finish using another handle. An `Arc` is Rust's shared ownership
pointer: cloning it gives another owner of the same value, and the value lives
until the last owner drops it. Here that lifetime protects rows and the write
operation rather than just saving an allocation.

Retries use the retained publication. If the chosen object key already belongs
to a different publisher, even one with identical samples, the store chooses a
new key instead of overwriting it. Query exclusion verifies the retained batch's
publication identity and encoded bytes; matching timestamps and values alone
can't establish ownership. A query keeps the pending rows while it examines the
archive, so cancellation, uncertain replies and listing races must still expose
one copy. New samples can join the buffer without becoming part of that frozen
batch. Retention must not discard the unresolved publication as old history.

For local writes, a visible directory name isn't a durable acknowledgement.
The writer confirms owned directory entries before creating descendants and
publishing the file, then confirms the published file's entry. A fresh store
must revisit an entry left visible by an uncertain earlier writer. This work is
bounded to the configured archive and its owned creation chain, not every
existing ancestor on the machine. The controlled fault tests check the order
and retained ownership. They don't simulate a broken disk or prove physical
power-loss survival; those results need the separate qualification record.

## Collecting metrics

The `sysinfo` crate gives us cross-platform system metrics without writing platform-specific code. On both Linux and macOS, we collect:

- **Node-level:** CPU usage, memory used/total, disk used/total, network rx/tx bytes and packets
- **Per-process:** CPU percentage and RSS memory for each running container (by PID)

One refresh call cost us a release candidate. `System::refresh_all()` looks like the obvious choice, but in sysinfo 0.33 it refreshes processes *without* removing the ones that have exited, and sysinfo keeps each tracked process's `/proc/<pid>/stat` file open so the next refresh is cheaper. Bun raises its open-file limit to about a million, so nothing capped that cache either. The V02 soak's workloads start a short-lived process every second or so, and after half an hour one node's Bun held 1,532 open files (992 of them `stat` files for processes long gone) and 951 MB of memory. The collector now refreshes memory, CPU and processes separately, asks for dead processes to be removed, and turns sysinfo's file cache off. `exited_processes_are_forgotten_on_refresh` starts twenty `sleep`s, lets one refresh see them, kills them, and checks the next refresh has forgotten every one; with `refresh_all` it still tracks all twenty.

Which units come back is the crate's choice, and it has changed its mind before: sysinfo reported memory in kilobytes until 0.26. In 0.2.2 we moved from 0.33 to 0.39, six breaking releases at once, and none of our calls stopped compiling. A silent change of unit was the real risk. A test like "total memory is positive" passes just as happily with KiB as with bytes, so the collector's tests now compare with a figure read without sysinfo. `node_memory_total_is_the_kernel_figure_in_bytes` checks `node_memory_total_bytes` against `MemTotal` in `/proc/meminfo` on Linux and `hw.memsize` on macOS, exactly. `process_memory_is_resident_bytes_not_kibibytes` checks a process's memory against `ps`, allowing for drift but not for a factor of 1,024. `process_cpu_is_percent_of_one_core_and_node_cpu_is_percent_of_all` spins one thread for a second and checks the process reads well above 1 (a 0–1 fraction couldn't), while node CPU stays between 0 and 100. `disk_capacity_is_bytes_from_the_filesystem`, in the diagnostics module, checks disk capacity against `statvfs`.

The bump did change one number on purpose. On macOS, sysinfo 0.38 counts used memory the way Activity Monitor does (app memory plus wired plus compressed), where 0.33 added up active, wired, compressed and speculative pages. So `node_memory_used_bytes` on a Mac now matches what you see in Activity Monitor. Linux still computes it as `MemTotal` minus `MemAvailable`. sysinfo 0.34 also turned its `multithread` feature off by default; we turn it back on, so the process scan still runs on a thread pool as it did before the bump.

Collection runs every 10 seconds. Each sample is a `(timestamp, metric_name, labels_json, value)` tuple, stored as an Arrow RecordBatch. When the batch fills up, it's flushed to a Parquet file.

## Prometheus scraping

Not everything comes from system stats. Your apps expose their own numbers (requests served, queue depth, how long a checkout takes) on a `/metrics` endpoint in the Prometheus text format. For a long time Reliaburger could only scrape a fixed list of URLs from the node config, which is fine for a node exporter and useless for an app with three replicas that move between nodes. Who writes those URLs? And who rewrites them after a deploy?

So an app now says where its metrics live, and the cluster works out the rest:

```toml
[app.web]
port = 8080
metrics = {}                     # scrape http://<instance>:8080/metrics
# metrics = { port = 9797, path = "/prom" }
```

`port` defaults to the app's port and `path` to `/metrics`. A Kubernetes manifest gets the same thing from the `prometheus.io/scrape`, `prometheus.io/port` and `prometheus.io/path` annotations on its pod template, which is how half the charts on Artifact Hub already say "scrape me". The podinfo demo declares `prometheus.io/port: "9797"`, so `relish apply -f podinfo.yaml` gives you `metrics = { port = 9797 }`, and the importer stops listing 9797 among the ports it drops.

### Every node scrapes its own

Prometheus runs one server that discovers every target and pulls from all of them. We already have a process on every node that knows exactly which instances it's running and at what address: Bun. So each Bun scrapes its *own* instances and nobody else's.

That choice pays for itself three times. There's no discovery to get wrong, because the agent that started the container is the one asking. The scrape never crosses the network: a runc container is reached at its container IP, the same address health checks use, and a process workload on loopback. The metrics port doesn't need publishing or routing, because the node is already inside the right network. And the samples land in the local Mayo store, next to that instance's CPU and memory, so the query fan-out we built for per-app metrics finds them with no changes.

The scrape loop runs beside the collector in `src/bin/bun.rs`. Every `app_scrape_interval_secs` (10 by default, the same as the collector, so an app's own series and its CPU line up) it asks the agent for targets over the command channel and does the HTTP work itself:

```rust
let (response, targets) = tokio::sync::oneshot::channel();
if scrape_cmd_tx
    .send(AgentCommand::ScrapeTargets { response })
    .await
    .is_err()
{
    break;
}
let Ok(targets) = targets.await else { continue };
scrape_app_targets(&scrape_mayo, &client, &targets, &scrape_node, timeout).await;
```

The agent answers from state it already holds (the deployed specs and each instance's container IP) and goes straight back to its loop. A hung app can stall the scrape task, but never the agent.

### A bounded fan-out with streams

A node might run forty instances. Scraping them one after another means one slow app delays the other thirty-nine; spawning forty tasks means no limit at all. The `futures` crate has exactly the tool:

```rust
let results: Vec<(AppScrapeTarget, Result<Vec<CollectedMetric>, ScrapeError>)> =
    futures_util::stream::iter(targets.to_vec())
        .map(|target| {
            let client = client.clone();
            async move {
                let result = fetch_metrics(&client, &target.url, timeout).await;
                (target, result)
            }
        })
        .buffer_unordered(MAX_CONCURRENT_SCRAPES)
        .collect()
        .await;
```

A *stream* is the async cousin of an iterator: it yields values over time, and you drive it with `.await` instead of a `for` loop. `stream::iter` turns our list into one. `.map` turns each target into a *future*, the not-yet-run work of scraping it (an `async move { ... }` block is an anonymous function body that runs later and takes ownership of what it uses). `buffer_unordered(16)` is the interesting part: it keeps at most sixteen of those futures running at once and hands results back in whatever order they finish. If you know Go, it's a worker pool with a semaphore, in one line. `.collect()` gathers everything into a `Vec`.

The first version borrowed `target` and `client` inside the future instead of owning them. It compiled as a plain function and failed the moment the loop ran inside `tokio::spawn`, with the wonderfully opaque "implementation of `FnOnce` is not general enough". The borrowed version ties each future's lifetime to the function's borrows, and the compiler can't prove that combination is `Send` (safe to move between threads, which the multi-threaded runtime requires). Giving every future its own copy fixed it. A `reqwest::Client` is an `Arc` inside, so cloning it costs a reference count, not a connection pool.

Each fetch is also bounded three ways: a timeout (half the interval, at most five seconds), an 8 MiB body cap, and a 20,000-sample cap. The endpoint belongs to the app, and a node shouldn't buffer whatever an app feels like sending. A scrape over either cap fails whole, the way Prometheus's `sample_limit` does: storing the first 20,000 samples would keep an arbitrary subset of the app's series and hide that anything was cut, so nothing is stored and `up` reads 0. `a_scrape_over_the_sample_limit_is_refused_whole` serves 20,001 samples and checks exactly that.

### Labels are the whole point

A sample with no labels is a number without a story. Every scraped sample gets four:

| Label | Example | Why |
|---|---|---|
| `app` | `default/web` | What every per-app query already filters on, and what process metrics use |
| `namespace` | `default` | Tenancy |
| `instance` | `default__web-0` | One line per replica on a chart |
| `node` | `node-02` | Where to go looking |

What if the app already sets `instance` itself? Prometheus renames the app's label to `exported_instance`, and so do we. Silently overwriting it would throw the app's data away; keeping it would mean two labels fighting over one name.

All samples from one sweep also share one timestamp. That sounds pedantic until you add series up: `http_requests_total{status="200"}` plus `{status="500"}` is the instance's total only if both were recorded at the same instant. The old code stamped each sample as it was inserted, so a scrape that straddled a second boundary split in two.

Finally, every target gets an `up` sample: 1 when the scrape worked, 0 when it didn't. A dead metrics endpoint then shows up as data you can query, not as a quiet gap.

### Histograms, stored properly

The previous parser had a bug hiding in one line:

```rust
prometheus_parse::Value::Histogram(buckets) => {
    buckets.iter().map(|b| b.count).sum::<f64>()
}
```

A Prometheus histogram's buckets are *cumulative*: `le="0.1"` counts requests up to 100 ms, `le="0.5"` counts those plus everything up to 500 ms, and so on. Summing them counts fast requests several times over and produces a number that means nothing. The fix stores what Prometheus stores, one series per bucket plus `_sum` and `_count`:

```rust
prometheus_parse::Value::Histogram(buckets) => {
    for bucket in buckets {
        let mut labels = labels.clone();
        labels.insert("le".to_string(), format_bound(bucket.less_than));
        push_finite(&mut metrics, format!("{}_bucket", sample.metric), labels, bucket.count);
    }
}
```

With `_sum` and `_count` side by side, mean latency over an interval is simply the increase in `_sum` divided by the increase in `_count`.

### Rates, and the counter that went down

Counters only go up. So when one goes *down*, the process restarted and started again from zero. `mayo::series::rates` turns a counter's points into per-second rates and treats a drop as a reset, the way Prometheus's `rate()` does:

```rust
pub fn rates(points: &[Point]) -> Vec<Point> {
    points
        .windows(2)
        .filter_map(|pair| {
            let [(before_at, before), (at, value)] = [pair[0], pair[1]];
            let elapsed = at.checked_sub(before_at).filter(|elapsed| *elapsed > 0)?;
            let increase = if value >= before { value - before } else { value };
            Some((at, increase / elapsed as f64))
        })
        .collect()
}
```

`windows(2)` walks a slice two elements at a time, overlapping: `[a, b]`, `[b, c]`, `[c, d]`. It hands you a borrowed sub-slice, not a copy, so there's no allocation. The next line *destructures* both pairs at once: `let [(before_at, before), (at, value)] = ...` pulls four named values out of an array of two tuples, the way Python's `(a, b), (c, d) = ...` does. The `?` after `filter(...)` works inside the closure because the closure returns an `Option`: no elapsed time means `None`, and `filter_map` drops that step.

This module is shared. `relish metrics` and the dashboard both start from the same raw rows and need the same arithmetic, so it lives in one place, away from HTTP and rendering, with unit tests for resets, gaps and division by zero.

### Reading it back

`relish metrics web` lists what was scraped, one number per metric, added up across instances:

```text
METRIC                         TYPE       SERIES  INSTANCES  VALUE
http_request_duration_seconds  histogram       2          2  mean 11.9ms
http_requests_total            counter         4          2  8.40/s
up                             gauge           2          2  2
```

How does it know a counter from a gauge without a `TYPE` line? By name, the way Prometheus's own conventions intend: `_total` is a counter, and `_sum`/`_count` are when they come as a pair. `--name` shows one metric per instance with a rate and a sparkline, and a histogram named by its base (`--name http_request_duration_seconds`) shows mean latency per instance. It asks for only the newest two samples of each series (`per_series=2`), which is plenty for a rate and cheap across a big cluster.

That parameter fixed a real bug on the way. The per-app query sorted oldest first and stopped at 10,000 rows, so on a busy app the newest samples, the ones anything called "latest" needs, were the first to be cut. The query now sorts newest first before the limit and flips the rows back, and the endpoint defaults to the last fifteen minutes instead of the beginning of time.

Each node still stops at 10,000 rows, so on a cluster the entry node merges answers that can cover different stretches of time: fifteen minutes from a quiet node, under a minute from one exporting thousands of series. The merge used to return that without comment, and a chart read the busy node's missing history as "the instance just started". Now a node whose answer is exactly at the limit is named in a `Truncated` warning, since a query that ends in `LIMIT` and comes back full may have left rows out. The same merge promised to drop duplicate rows but sorted on the timestamp alone, and `Vec::dedup_by` only compares neighbours, so two equal rows with a different row between them both survived. It now sorts on the whole identity, `(timestamp, metric_name, labels)`, comparing tuples of references so nothing is cloned to build the key, and equal rows end up side by side.

The dashboard's app charts had a bug of their own: `brioche.js` expected an array, the per-app endpoint answered `{data, warnings}`, and the script returned early. Every app chart was empty, always. They now read from a small chart endpoint that returns series already lined up per instance, `{timestamps, series: [{label, values}]}`, with counters as rates and histograms as means, so the browser only draws. A Rust test pins that shape, because that's the contract the JavaScript relies on. An app with scraped metrics also gets a requests-per-second chart (its `http_requests_total`, or failing that its own first counter) and a latency chart.

Configure fixed URLs with `[[metrics.scrape_targets]]` when there's no app to hang them on (a node exporter, say); those keep their `job` as the `app` label.

## Alert evaluation

Four built-in alert rules catch the most common failure modes:

1. **cpu_throttle** — CPU above 90% for 5 minutes (critical)
2. **oom_risk** — memory above 85% for 2 minutes (critical)
3. **memory_high** — memory above 70% for 10 minutes (warning)
4. **disk_high** — disk above 80% for 5 minutes (warning)

There used to be a fifth, `cpu_idle`: node CPU below 5% for 30 minutes, labelled "possible zombie". It looked sensible on paper. Then a healthy three-node cluster in the Wyse lab went `DEGRADED` half an hour after we claimed it, because two of its nodes had nothing to do: the only app ran on the third. An idle node isn't broken. It's spare capacity, and every new or quiet cluster looks exactly like that. Since any firing alert turns `relish status` and the dashboard `DEGRADED`, the rule cried wolf on the clusters that were doing best, so we dropped it in 0.3.1 ([#2171](https://github.com/reliaburger/reliaburger/issues/2171)). A test now feeds an idle node's readings through the default rules, with every `for` window set to zero, and checks that nothing fires.

The zombie it was hunting is real, though. It's an *app* that holds a CPU reservation and does no work, and a node has no reservation to waste. So zombie detection belongs per app, comparing each instance's CPU use with its reservation. That's later work.

The alert state machine is simple: Inactive → Pending → Firing. A metric breaches its threshold, the alert goes to Pending. If the breach persists for the required duration, it fires. If the metric recovers, the alert goes back to Inactive. No hysteresis, no complex inhibition rules. Just thresholds.

## Ketchup: where the logs go

Every line that a container writes to stdout or stderr ends up in Ketchup. The first design gave each app one plain-text file per day, `{logs_dir}/{namespace}/{app}/{date}.log`, with every line prefixed by a timestamp and an `O` or `E` for the stream:

```
1704067200 O starting up
1704067201 E warning: config file not found, using defaults
1704067202 O listening on :8080
```

A sparse timestamp index was to sit beside each file, one `(byte_offset, timestamp)` entry every 4 KB, so a query could binary-search to its start time and scan forward. And Ketchup would sniff the first ten lines for JSON and mark the stream as structured.

We never shipped that. Ketchup buffers lines in memory and flushes them to flat Parquet files in the logs directory, `logs_000000.parquet`, `logs_000001.parquet` and so on (`src/ketchup/log_store.rs`), with the stream as a column instead of a prefix. The next section explains why. There's no per-line index and no JSON detection on the node: `relish logs api --json-field level=error` parses each line as JSON on the client and keeps the ones whose field matches:

```bash
relish logs api --json-field level=error
```

## SQL over logs

Here's something you don't see in most observability stacks: the same SQL engine that queries your metrics also queries your logs.

Ketchup stores logs in the same Arrow/DataFusion/Parquet stack that Mayo uses for metrics. The schema started as five columns: `timestamp`, `app`, `namespace`, `stream`, and `line`. (It has seven now; "Tails that tell the truth" below explains the other two.) Want to find all errors from the web app in the last hour?

```sql
SELECT timestamp, line FROM logs
WHERE app = 'web'
AND timestamp > 1704067200
AND line LIKE '%ERROR%'
ORDER BY timestamp
```

No new query language. No log-specific DSL. Just SQL.

### Why columnar storage works for logs

You might think logs are just text, so columnar storage wouldn't help. But most of the data in a log line isn't the message — it's the metadata. The `app` column for 10,000 lines from the same app stores "web" once in a dictionary and references it 10,000 times. The `namespace` and `stream` columns work the same way. Timestamps delta-encode beautifully.

Even the `line` column compresses well. If your app is stuck in an error loop printing the same stack trace 10,000 times, Parquet's dictionary encoding stores it once. An error loop that eats 2MB as flat text might be 10KB in Parquet.

Overall, expect 3-5x compression versus flat log files.

### How LIKE queries work without full-text indexes

When you write `WHERE line LIKE '%ERROR%'`, DataFusion doesn't have an inverted index to consult. It scans the `line` column. But columnar storage makes this much faster than grep on a flat file:

1. **Columnar pruning.** DataFusion only reads the `line` column, not timestamp/app/namespace/stream. That alone can skip 60% of the data.

2. **Predicate pushdown.** A query like `WHERE app = 'web' AND timestamp > X AND line LIKE '%ERROR%'` filters by app first (dictionary lookup, instant), then by timestamp (range check), and only scans `line` for the surviving rows. If 99% of rows are eliminated before the LIKE, the scan is tiny.

3. **Row group statistics.** Parquet files are split into row groups. Each group stores min/max values per column. A time-range query can skip entire groups without reading them.

This isn't a full-text search engine. If you need to search millions of unique log lines by arbitrary substring, you'd want something like Elasticsearch. But for the common case — filter by app and time first, then grep — it's fast.

A future improvement: Parquet supports bloom filters per column. Writing a bloom filter on the `line` column during flush would let DataFusion skip row groups that definitely don't contain the search term.

### From LIKE to a regular expression

`relish logs --grep` started as that `LIKE '%…%'`, a substring. People typed `--grep 'error|warn'` anyway, because that's what grep means everywhere else, and got nothing back. So `--grep` is now a regular expression, and the store's condition is `regexp_like(line, '…')`.

Two details made it more than a one-word change. First, a pattern like `(unclosed` doesn't compile, and DataFusion reports that as a planning error, which the API used to pass on as a 500. That reads as "the log store is broken" when the user only mistyped. So the store checks the pattern first with the `regex` crate and returns `KetchupError::QueryRejected`, which the handlers turn into a 400 with the reason, and `relish logs` checks it again before sending anything. It's the same engine DataFusion runs underneath `regexp_like`, so the check and the query can't disagree about what's valid. A cluster-wide query checks once, before the fan-out. Otherwise every node would refuse the pattern separately, and you'd get an empty result buried under one warning per node.

Second, the client filters too, for `-f` and for `--json-field`, which the server can't do. A follow can carry thousands of lines, and compiling the pattern for each one would be silly, so `LogOptions::line_filter` compiles it once into a `LineFilter`. `regex::Regex` holds the compiled automaton and `is_match` borrows it immutably, so one value can serve every line without a lock. If you're coming from Python, it's `re.compile` hoisted out of the loop, except that the compiler makes you decide where it lives.

`--instance`, `--until` and `--stream` came with it. Both are plain conditions in the same `WHERE` (`instance = '…'`, `timestamp <= …`), and both travel with the fan-out to every node. `--instance` also works with `-f`: the agent's `FollowLogs` keeps only that instance's capture. The agent's plain-text fallback, used when the store has nothing yet, can't tell instances or times apart, so with any of them set, `relish logs` skips it rather than print lines the filter should have hidden. `--stream stderr` is a third plain condition, `stream = 'stderr'`. It isn't offered with `-f` yet, because a followed tail is read raw from the capture files, which don't say which stream each line came from; refusing beats a filter that silently lets some lines through.

### The unified query path

Both the flushed Parquet files and the unflushed in-memory buffer are included in every DataFusion query. Same trick we use for metrics. There's no blind spot — you see logs from 30 seconds ago in the same SQL query as logs from last week. No merging, no separate code paths, no seams.

A failed flush used to drain the only in-memory copy before it wrote Parquet. An unwritable directory or a failed ingest-checkpoint rename then lost captured rows while their live offsets still claimed they had been read. The store now owns one pending batch until both its Parquet file and capture checkpoint are durable. Retrying reserves the same filename, serialises writers for that batch and finishes it before advancing a newer checkpoint. An async caller can be cancelled while its blocking writer runs; that writer retains its I/O guard, and the store retains the rows for the next attempt.

Queries keep their own copy of the pending batch and exclude its immutable
sequence range from the disk relation. A rename during the disk scan therefore
cannot add a second copy. New rows are allocated and inserted together under the
store's write lock, so they always lie above that reserved range. Cancellation
and checkpoint faults must leave those retained rows queryable until retry
finishes the acknowledgement.

The restart boundary needs evidence in the published file too. The Parquet
footer carries the batch's frozen capture checkpoint; the JSON sidecar carries
the same identity and offsets. Reopening can recover the published checkpoint
when a process stopped between Parquet publication and the sidecar update.
That closes a different gap from keeping an `Arc` alive: no in-memory pointer
survives process loss.

Capture identity comes from the same opened file that supplied the bytes and
travels with its position into both checkpoint representations. Re-statting a
pathname later can bind an old offset to a replacement file and skip its new
prefix. The controls replace that path after the old line was read, then require
the replacement to start at zero. They also hold directory confirmation and
checkpoint publication separately. These are the specified append-only capture
and restart guarantees; they don't make arbitrary file rotation or unflushed
memory durable. The [final codebase qualification record](../qualification/2026-10-04-final-codebase-qualification.md)
keeps the actual execution and physical interruption results pending until the
matching gates run.

## The dashboard

Brioche is a single HTML page. No React, no Vue, no webpack. The server renders the HTML with current data, embeds a 2KB CSS stylesheet, and sends it. The browser refreshes every 5 seconds via a `<meta http-equiv="refresh">` tag.

The dashboard shows three sections: apps (name, status, instance count), nodes (name, state, app count), and alerts. Status dots are green for healthy, amber for pending, red for failed. (From 0.2.8 an app's status is the word `relish apps` uses, `running`, `degraded`, `down` or `stopped`, from one shared rollup; see [Built for Humans and Their Agents](19-built-for-agents.md#brioches-dashboard).) The dark theme is easy on the eyes during those late-night debugging sessions.

Total payload: under 10KB. First paint: instant.

## Under the hood: key patterns

### Arrow RecordBatch construction

Each metrics sample starts as a Rust struct. To get it into DataFusion, we transpose the data into columnar arrays and wrap them in a `RecordBatch`:

```rust
fn buffer_to_batch(&self) -> Result<Option<RecordBatch>, MayoError> {
    if self.buffer.is_empty() {
        return Ok(None);
    }

    let timestamps: Vec<u64> = self.buffer.iter().map(|s| s.timestamp).collect();
    let names: Vec<&str> = self.buffer.iter().map(|s| s.metric_name.as_str()).collect();
    let values: Vec<f64> = self.buffer.iter().map(|s| s.value).collect();

    let batch = RecordBatch::try_new(
        Arc::new(metrics_schema()),
        vec![
            Arc::new(UInt64Array::from(timestamps)),
            Arc::new(StringArray::from(names)),
            Arc::new(Float64Array::from(values)),
        ],
    )?;
    Ok(Some(batch))
}
```

Four iterations over the same buffer, producing four column vectors. If you're coming from Python, think of it as converting a list of dicts into a dict of lists — the same data, rotated 90 degrees. Each column becomes an `Arc<dyn Array>` because DataFusion needs shared ownership (multiple query operators might read the same batch concurrently).

The `?` on `try_new` catches schema mismatches: if you pass three arrays when the schema expects four, you get an error at batch construction time, not somewhere deep in a query plan. Fail fast.

### The alert state machine

The alert evaluator has three states, all decided in a single `match`. The first version matched on a boolean `breaching` flag. That had a bug we'll come back to, so here's the version we actually ship, which matches on the metric value itself (an `Option<f64>`):

```rust
let new_state = match (&prev_state, value) {
    // No data at all: keep firing (can't prove recovery), else go inactive.
    (_, None) => match &prev_state {
        AlertState::Firing { .. } => prev_state.clone(),
        _ => AlertState::Inactive,
    },
    (AlertState::Inactive, Some(v)) if rule.operator.eval(v, rule.threshold) => {
        AlertState::Pending { since: now }
    }
    (AlertState::Pending { since }, Some(v)) if rule.operator.eval(v, rule.threshold) => {
        if now.duration_since(*since).unwrap_or_default() >= rule.for_duration {
            AlertState::Firing { since: *since }
        } else {
            prev_state.clone()
        }
    }
    (AlertState::Firing { .. }, Some(v)) if rule.operator.eval(v, rule.threshold) => {
        prev_state.clone()
    }
    // Data present and back in range: a genuine recovery.
    (_, Some(_)) => AlertState::Inactive,
};
```

The `since` field is set when the alert enters Pending and preserved when it moves to Firing, so you know when the breach *started*, not when it was confirmed. The `if rule.operator.eval(...)` bits are *match guards*: a `match` arm only fires when its pattern matches *and* the guard is true. Rust checks that the arms are still exhaustive with guards in place, so nothing slips through.

If you're used to state machines in Go or Java, this might look too compact. Where are the separate `handleInactive()`, `handlePending()`, `handleFiring()` methods? Rust's pattern matching collapses them into one expression, and the compiler ensures you handle every combination. Add a fourth state and every `match` in the codebase that doesn't handle it becomes a compilation error.

## Hardening the metrics path

The first cut of Mayo worked in the demo and passed its tests. A later review found five sharp edges that only bite in production, not in a thirty-second demo, and a long soak found a sixth. They're worth walking through, because each one is a small change that fixes a whole class of failure.

**SQL injection through a metric name.** The per-app query endpoint built its SQL by pasting the caller's `?name=` and the app's `namespace/app` straight into the string. Send `?name=x' OR '1'='1` and the injected quote closes the literal early, drops the tenant and time predicates, and hands back every app's metrics. The fix is the same one every database driver ships: escape the value. DataFusion follows standard SQL, so a `'` inside a literal is doubled:

```rust
pub(crate) fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}
```

Now `x' OR '1'='1` is matched *literally*, finds no metric by that name, and returns nothing. We escape every interpolated value, including the app and namespace, not just the obvious one.

**Rollups that double-count.** Each worker rolls up its last minute of metrics and pushes the summary to a council member. After a reassignment a worker re-sends its recent windows as backfill, so the same `(node, window)` can arrive twice. The council was summing both, inflating the cluster total. Two changes fix it. First, we align every window to a minute boundary (`now - (now % 60)`), so two nodes ticking a few seconds apart stamp the same minute with the same timestamp. Second, we make ingest idempotent, keyed on `(node_id, window)`:

```rust
let key = (node_id.clone(), rollup.timestamp);
if !self.seen_windows.insert(key) {
    return false; // already ingested this window; drop it
}
```

`HashSet::insert` returns `false` if the key was already present, which is exactly the "have I seen this?" question we need in one call. (The key has since grown a third part, the rollup's part number, for minutes too big for one message; [Eyes Everywhere](11-eyes-everywhere.md#the-rollupworker) has that story.)

**Temp files that outlived a crash, and a sync nobody checked.** A metrics flush writes a randomly named `.mayo-pending-*` file and renames it into place. A crash between the two left the temp file for good: nothing deleted it and the size cap didn't count it (#2059). `MayoStore::new` now deletes them, which is safe because one Bun owns the directory. The rollup writer had the opposite slip. It renamed its file and then synced the directory, but threw away a failed sync and returned `Ok`, so the caller cleared its buffer while the rename could still vanish in a power cut (#2060). The error now propagates and the rows wait for the next flush. That alone left a second bug, which review caught: the renamed file stayed in place while the rows waited, so queries counted them twice until the retry. So the writer removes the file before it reports the failure. A filesystem that can't sync a directory at all answers `EINVAL`, `ENOTSUP` or `ENOSYS`, and that isn't a failed flush: the bytes are synced and there's nothing to retry. The test takes read permission off the directory (`0o300`: write and search only), so creating and renaming the file still work and only opening the directory to sync it fails.

**Rollups that vanished on restart.** The rollup store kept its flushed data in an in-memory `Vec<RecordBatch>` and named every Parquet file with a counter that reset to zero on start. So a restart both lost all history *and* overwrote `rollup_000000.parquet` with new data. We fixed both by making the rollup store read its history back from the Parquet directory (like the metrics store already did) and by seeding the flush counter one past the highest file on disk. Restart now recovers everything and appends rather than clobbering.

**Per-app metrics that were never collected.** Production only collected node-level metrics (CPU, memory for the whole box). The autoscaler and the per-app dashboards had nothing to read. The collector already knew how to scrape a single process; it just wasn't being called. The collection loop now asks the agent for its running instances and collects per-process CPU and memory for each one, labelled `namespace/app`. (Even then the autoscaler kept asking for a series called `cpu` that nobody records. Chapter 9 tells that story. The per-process memory series is what memory autoscaling reads; Chapter 9 also shows it driving a real scale-up on runc.)

**A flush that froze every query.** The flush wrote Parquet while holding the store's write lock, and Arrow's writer is synchronous. So for the duration of the write, every query waited. Worse, blocking I/O on an async task stalls the whole tokio runtime. The first repair split the flush in two: prepare the batch under a brief lock, then write outside it, on the blocking pool:

```rust
let pending = { store.write().await.take_flush_batch()? };  // brief lock
if let Some(p) = pending {
    write_pending_flush(p).await?;  // no lock held; runs on spawn_blocking
}
```

The current ownership contract also keeps that prepared batch in the store until
acknowledgement completes. Moving the write outside the lock mustn't move the
only copy outside the store. While the write is in flight, queries hold a read
lock and proceed. And a corrupt or truncated Parquet file (a flush killed mid-write) no longer poisons the directory: we read each file on its own and skip the bad one with a log, so one botched flush doesn't fail every unrelated read.

**Reads that grew with uptime.** After 0.1.0 a long soak showed Bun's memory climbing for hours without ever tripping the leak check. Nothing leaked. A test binary with its own counting allocator (a `#[global_allocator]` that forwards to `System` and adds up the bytes, so it counts the heap we ask for rather than what the allocator keeps mapped) showed each alert evaluation and rollup giving back every byte it took. What grew was the *peak*: about 1.5 MiB more per hour of history, because a query for the last two minutes still loaded every Parquet file in the directory before filtering. With seven days of retention that's a week of growth. A read with a lower bound now skips any file whose newest sample, read from the footer statistics, is older than the bound:

```rust
if since.is_some_and(|since| file_max_timestamp(&path).is_some_and(|max| max < since)) {
    continue;
}
```

A file with no usable statistics is still read: skipping it would silently drop data. That made the reads that run on a timer flat. It didn't touch the rest.

The theme across all of these: the happy path was fine, and the failure paths — an attacker, a reassignment, a restart, a dead app, a crash mid-flush — were where the bugs lived. That's usually where they live.

## Streaming the queries

Ask for `/v1/metrics?name=*` with no start, or a window from yesterday morning, or just the list of metric names, and the store still decoded every file at or after the start into one in-memory `MemTable` before DataFusion saw a single row. The council's rollup store had no bound at all. And a node writing to a bucket fetched every object for every query, the periodic ones included. We extended the counting-allocator test to all of those, adding history an hour at a time. Every one of them needed between two and three times the heap at four hours that it needed at one.

The answer isn't a smarter filter in front of the `MemTable`. It's to stop building one.

**A table that's a source.** DataFusion asks a table for an execution plan through the `TableProvider` trait, and when it does, it says three things: which columns the query uses, which filters it applies, and any row limit. Our `ParquetTable` (in `src/mayo/scan.rs`) answers with a plan whose one partition reads the files lazily, one after another, then the buffer:

```rust
let files = futures_util::stream::iter(0..sources.len())
    .then(move |index| read_source(sources[index].clone(), Arc::clone(&plan)))
    .flat_map(|read| { /* the file's batches, or its error */ });
```

A `Stream` is the async cousin of an iterator: a sequence whose next item you `.await`. If you've written Go, it plays the part of a channel fed by a goroutine, except nothing runs until the consumer asks for the next item. `stream::iter` turns any iterator into one. `then` maps each item through an async function and waits for that future to finish before it starts the next, so at most one file's decoded batches sit in the scan at any moment. `flat_map` flattens each file's `Vec` of batches back into a stream of single batches.

DataFusion's operators pull from that stream as it goes. A filter passes rows through. A hash aggregate keeps one accumulator per group, not the rows behind it, so a `GROUP BY` over a week holds a few dozen sums. An `ORDER BY timestamp LIMIT 10000` runs as a top-k that keeps ten thousand rows and drops the rest as they arrive. That's the incremental half of the fix, and we got it for free once the rows arrived one file at a time.

**Pushdown from the footer.** The other half is not reading what can't match. A Parquet footer holds the minimum and maximum of every column in every row group, so before decoding anything, `read_source` checks the query's bounds against those statistics:

```rust
fn admits_times(&self, min: Option<u64>, max: Option<u64>) -> bool {
    if let (Some(since), Some(max)) = (self.since, max)
        && max < since
    {
        return false;
    }
    // …and the same for `until` against `min`
    true
}
```

A row group that fails isn't decoded, and a file with no row group left isn't read past its footer. For a bucket the footer costs two range reads (the last eight bytes hold its length and the `PAR1` magic, then the metadata itself), so an object outside the window never downloads at all. The decoder also reads only the columns the query named: `SELECT DISTINCT metric_name` decodes one column of four.

Where do the bounds come from? DataFusion hands `scan` the query's filters as expression trees, and `ScanBounds::from_filters` walks them. `timestamp >= 100` raises `since`, `metric_name = 'cpu'` narrows the names, an `AND` narrows by both sides. `OR` needs more care. A row matching either side lies within the *hull* of the two sides' bounds:

```rust
fn hull(self, other: Self) -> Self {
    Self {
        since: self.since.zip(other.since).map(|(a, b)| a.min(b)),
        until: self.until.zip(other.until).map(|(a, b)| a.max(b)),
        names: self.names.zip(other.names).map(|(a, b)| a.union(&b).cloned().collect()),
    }
}
```

`Option::zip` pairs two options: `Some(a).zip(Some(b))` is `Some((a, b))`, and anything with a `None` in it is `None`. So if either side of the `OR` is unbounded in time, the hull is too, which is exactly right. We didn't plan to handle `OR` at first. Then the test for `metric_name IN ('cpu', 'mem')` came back with no names at all: DataFusion's optimiser rewrites short `IN` lists as a chain of `OR`s before the table ever sees them.

Anything `from_filters` doesn't recognise (a `LIKE`, a function call, a cast) gives no bound, and the table reports every filter as `Inexact`, which tells DataFusion to keep applying the real filter on top of the scan. So pruning only ever has to be safe, never complete. The labels filter is a substring match on a JSON string, which a min and max can't answer, so it stays a streaming filter.

**The crate chapter 2 did without.** Back in Chapter 2 we made a point of writing async trait methods as `fn … -> impl Future` instead of pulling in the `async_trait` crate. `TableProvider` is declared with `#[async_trait]`, so implementing it means adding the crate and putting the same attribute on our `impl`. Why would DataFusion choose the older style? Because it keeps tables as `Arc<dyn TableProvider>`, a *trait object*, Rust's equivalent of a Go interface value: a pointer to the data plus a table of method pointers. That table needs one concrete signature per method, and with `impl Future` every implementation returns a different future type. `#[async_trait]` rewrites `async fn scan` into a method returning `Pin<Box<dyn Future<Output = …> + Send>>`, one type for all of them, at the cost of a heap allocation per call. One allocation per query is nothing next to decoding a Parquet file.

**The repartition surprise.** With streaming in, the periodic cycle's peak fell. The metric-names query and a per-series aggregate still grew, though: not by much, but steadily, hour after hour. DataFusion splits work across a partition per CPU, and for an aggregate it puts a repartition step after the scan, a task that pulls from our stream and deals batches out to the partitions. That task doesn't wait for slow consumers, so batches queued behind them. Setting `target_partitions` to one removed the step, and the peaks fell by an order of magnitude: the alert-and-rollup cycle went from about 4 MiB to about 300 KiB. Mayo's queries are per node and small, so one thread reads them comfortably.

**Proving the answers didn't change.** A faster wrong answer is still wrong. Before touching the read path we recorded golden answers: every read both stores offer, over data with overlapping files, out-of-order backfill, a corrupt file and an unflushed buffer, run through the old eager code and saved as `insta` snapshots. The streamed code matches them, on the local backend and the bucket. A property test then throws random file layouts and random queries (ranges, strict bounds, `IN`, `OR`, `NOT`, an aggregate) at both the streamed scan and an eager reference.

Would it catch a bug, though? We checked by planting one: make `timestamp > a` prune from `a + 2` instead of `a + 1`. The first version of the property test passed anyway. Its timestamps spread over a range of two hundred, so a bound almost never landed exactly on a file's last sample, which is the only place that off-by-one shows. Shrinking the data (sixteen distinct timestamps, files of one to four rows) made it fail on every run. A property test is only as good as the inputs it generates, so it's worth breaking the code once to find out.

That property test later timed out at nextest's 60-second limit on a busy Mac (#751). It wasn't the queries. Each of its 256 cases wrote up to seven Parquet files, and every flush makes several `sync_all` calls: the file, the data directory and any parent it created. On Apple platforms Rust implements `sync_all` with `fcntl(F_FULLFSYNC)`, because a plain `fsync` there only hands the bytes to the drive, which may still hold them in its cache. `F_FULLFSYNC` waits for the cache to empty. We measured about 9 ms for one against 0.12 ms for `fsync`, and it gets slower with every other process syncing the same disk. Alone, the test spent 85% of its 16 seconds waiting rather than computing; eight copies at once took 71 to 76 seconds each.

The syncs are what make a flush durable, so they stay. The test changed instead. A file layout is the expensive part of a case and a query is the cheap part, so each case now builds its files once and runs all ten query shapes against them, each with its own bounds and names. Thirty-two cases cover every shape against every layout with about an eighth of the flushes. Eight copies at once now take 13 to 15 seconds. Both planted bugs (the `a + 2` one above, and pruning a file whose last sample equals the lower bound) still fail on every run. If a test's time goes to waiting for the disk, ask whether each case needs its own files before reaching for a seam that skips the syncs.

What still grows with history? The list of file names a query starts from, a couple of hundred bytes a file, which is a couple of MiB for a week of one-minute flushes. And a query whose *answer* grows with history, like `/v1/metrics?name=X` with no start, needed room for that answer, until the next section gave every answer a ceiling. Everything else is flat.

### Ten thousand footers

Streaming fixed memory, but not time. Each flush writes one file, a flush runs every minute, and retention keeps seven days, so a node settles at about 10,080 `metrics_NNNNNN.parquet` files. The scan decides whether a file can match from its footer statistics, and to read the footer it has to open the file. So the alert check, asking every 30 seconds for the last two minutes, opened ten thousand files to read rows from two of them. The rollup worker did the same every minute. On a Wyse 3040's eMMC that's seconds of I/O per query, for ever; on the object-store backend, two ranged GETs per object per query ([#2425](https://github.com/reliaburger/reliaburger/issues/2425)).

The file name doesn't help (it's a counter, not a time), but the file never changes once it's written. That's the property that makes a cache trivially correct: whatever a footer said, it will always say. So each store keeps a `SourceIndex`, a map from file to its `(min, max)` timestamp, behind an `Arc<Mutex<…>>` that the store, every snapshot of it, and the scan's blocking threads share. A flush records the range of the batch it's about to write, so a new file is never opened just to learn its range. Any other file, after a restart say, gets its entry the first time a query reads its footer. Before opening a source, the scan asks the index:

```rust
if let Some(range) = index.range(&described)
    && !plan.bounds.admits_times(Some(range.min), Some(range.max))
{
    return Ok(Vec::new());
}
```

That `if let ... && ...` is the let chain from Chapter 1. Prune deletes a file and forgets its entry, and reads a known file's newest timestamp from the index instead of its footer.

The mutex is `std::sync::Mutex`, not tokio's. The project's rule is that a std mutex is for short synchronous critical sections only, and these are: one `HashMap` lookup or insert, never held across an `.await`, and taken from blocking threads too, where tokio's async lock would be awkward. The test `a_recent_window_query_reads_only_files_that_overlap_it` writes 50 one-minute files and counts footer reads for a one-minute query: at most two, where it used to be fifty. `a_reopened_store_reads_each_footer_once` pins the restart case, `a_recent_window_remote_query_skips_old_objects` the bucket, and `a_pruned_file_leaves_the_index` the cleanup.

What it doesn't fix is the file count. Listing the directory still grows with history, and a query that *wants* a week still opens a week of files. Compacting minute files into hourly ones would bound both, but it changes what's on disk, so it's its own piece of work.

### Every read gets a ceiling

Streaming bounded the memory a query *works* in. It didn't bound what a query *returns*, or how long it runs, and the November 2026 audit found seven read routes where the caller chose both ([#2382](https://github.com/reliaburger/reliaburger/issues/2382)). The commonest was the most innocent: `relish logs web` with no `--tail`. Every node built `SELECT ... FROM logs WHERE app = 'web' ... ORDER BY sequence`, sorted the app's whole history, collected it into a `Vec<LogEntry>` and serialised it. A chatty app with a 2 GiB log store could get Bun killed for memory by anyone allowed to read its logs, and Bun supervises every workload on the node.

The fix is one rule, applied to logs and metrics alike. Every query has a row cap, a maximum, and a deadline, and an answer that hits a cap the caller didn't choose says so.

- A log query with no tail now returns the newest `DEFAULT_LOG_QUERY_ROWS` (10,000) lines; a tail above `MAX_LOG_QUERY_ROWS` (100,000) is clamped. The SQL always carries the `ORDER BY sequence DESC LIMIT n` that `--tail` used to add, which DataFusion runs as a top-k: it keeps n rows and drops the rest as they stream past. The query also runs under the 256 MiB memory pool that `/v1/logs/sql` already had.
- `/v1/metrics?name=X` and `/v1/metrics/rollup?name=X` keep the newest 10,000 rows, the same newest-first-then-flip trick the per-app query uses.
- `/v1/metrics/summary` and `/v1/metrics/keys` list the names seen in the last hour (`METRIC_NAMES_WINDOW_SECS`) instead of every name in seven days of files.
- Every user-driven query runs inside `tokio::time::timeout` with an 8-second deadline, under the fan-out's 10-second wait, so a node says "narrow the query" before the caller gives up on it.

The deadline is a small function, and it's the first place this chapter drops a future on purpose:

```rust
async fn within_deadline<T>(
    deadline: std::time::Duration,
    query: impl std::future::Future<Output = Result<T, KetchupError>>,
) -> Result<T, KetchupError> {
    tokio::time::timeout(deadline, query)
        .await
        .unwrap_or_else(|_| Err(KetchupError::QueryRejected { /* ... */ }))
}
```

`timeout` returns `Err(Elapsed)` if the inner future hasn't finished in time, and then drops it. In Rust, dropping a future *is* cancelling it: a future only makes progress while someone polls it, so once nobody holds it, the DataFusion stream inside stops pulling files. No cancellation token, no flag to check. (A file already being decoded on the blocking pool finishes that one file, and its batches are thrown away.) `impl Future<Output = ...>` in argument position means "any type that is a future yielding this", the same shorthand as `impl Trait` anywhere else, so the function takes an `async` block directly.

A refused query is `KetchupError::QueryRejected` or the new `MayoError::QueryRejected`, which the handlers turn into a 400 with the reason. The caller asked for too much; retrying won't help, narrowing will.

And the cut is visible. When a cross-node log answer reaches the default cap, or a tail was clamped, the result carries `LogQueryWarning::Truncated { limit }`. `relish logs` already prints every warning, so the operator sees "showing the newest 10000 lines; use --since or --tail to choose others" without the CLI changing at all. An explicit `--tail 50` that the answer fills is not a cut: you asked for fifty. The tests pin each piece: `a_query_without_a_tail_returns_at_most_the_default_row_cap`, `a_named_query_returns_at_most_the_row_limit`, `a_named_rollup_query_returns_at_most_the_row_limit`, `a_tail_above_the_maximum_is_refused_or_clamped`, `a_log_query_cut_at_the_default_cap_says_so`, and two deadline tests that hand `within_deadline` a future that never finishes (`std::future::pending()`) and check the refusal arrives in time.

## Hardening the log path

Ketchup had the same shape of problem: a demo-clean happy path and a set of failure paths nobody had walked. The same review found five.

**A raw-SQL endpoint with no seatbelt.** `GET /v1/logs/sql?q=…` handed the caller's SQL straight to DataFusion. That's a lot of rope: `SELECT * FROM logs` streams the whole archive back through one response, an unbounded aggregation can exhaust the agent's memory, and a `DROP`/`INSERT` shouldn't be reachable from a read endpoint at all. The bounded path fixes all three. It accepts only a read-only `SELECT`/`WITH`, runs against a session that registers just the `logs` table (so a reference to any other table fails to plan rather than reading it), wraps the query in an outer `LIMIT` to cap rows, and runs under a working-memory limit so a runaway sort *errors* instead of taking the node down:

```rust
let bounded = format!("SELECT * FROM ({trimmed}) AS bounded LIMIT {MAX_LOG_SQL_ROWS}");
```

A rejected query is a `400`, not a `500` — the client asked for something the endpoint won't do, and the error says which.

**Fan-out that hid the dead.** When an app runs on several nodes, the leader asks each node for its slice of the logs and merges the answers. The first version turned *every* failure — a node down, a non-2xx status, unparseable JSON, a panicked task — into an empty success. So "this app produced no logs" and "half your cluster is unreachable" looked identical. Now fan-out returns a partial result: the entries it *did* collect, plus a list of which nodes failed and why. The caller can tell the two apart.

**A merge that deleted real events and kept fakes.** The merge deduplicated only *adjacent* equal `(timestamp, line)` pairs after sorting. Two problems. If a third line at the same timestamp sorted between two genuine duplicates, they weren't adjacent and both survived. And two replicas that each logged an identical line at the same instant — two real, distinct events — sorted adjacent and got collapsed into one. The fix is to dedup on a stable identity that includes *which node* produced the line: `(node, timestamp, stream, line)`. The same event reported twice by one node collapses; the same line from two replicas is two events and both survive.

**A filesystem "copy" pretending to be an object store.** The export config accepted `s3://` and `gs://` URLs, but the code turned the destination into a `PathBuf` and called `std::fs::copy`. An S3 target silently wrote to a local directory named `s3:`. Ketchup already had the right tool in the tree — `object_store`, which the snapshot uploader uses — so export now parses the destination through `object_store::parse_url` and `put`s each file. A bare path still works (we normalise it to `file://`), so nothing in the local tests changed; `s3://` and `gs://` now mean what they say.

**A checkpoint that skipped reused filenames — and a second one behind its back.** Export is incremental: a checkpoint records which files have shipped so a restart doesn't re-send everything. The old checkpoint keyed on filename. But log files are named `logs_NNNNNN.parquet` from a counter that resumes past the highest file on disk, so once retention prunes every file the counter resets to zero and a later flush reuses `logs_000000.parquet` for *different* bytes — which the filename-keyed checkpoint skips forever. We key the checkpoint on a durable id instead: the filename plus a hash of its contents, so a reused name with new bytes is a new object. And `relish logs-export` used to keep its own competing checkpoint in the same directory; both now share one Bun-owned file, so a manual export and the agent's export loop can't skip or double-ship each other's work.

**A checkpoint that only saved on success.** The export kept its checkpoint in memory while it uploaded file after file, and wrote it to disk once every file had gone. A failed read or `put` returned through `?` before the save, and the in-memory copy, holding every file uploaded so far, was dropped with it. Files go in name order, so the next run uploaded the same early files again, failed somewhere later, and the checkpoint never moved. Because disk-pressure pruning waits for an export to work, the log directory then grew past its cap. `?` is the right default, but it returns *now*, skipping everything after it in the function, cleanup included. Here the cleanup mattered, so `export_logs` keeps the `Result` in a variable, saves the checkpoint either way, and returns the export's error afterwards. `a_failed_export_keeps_the_files_it_already_uploaded` makes the second of two files unreadable and checks the checkpoint on disk names the first.

**A doc comment describing a feature that didn't exist.** `relish logs-export`'s doc said it "triggers an immediate export from the running Bun agent's LogStore" and "falls back to direct file copy if the agent is unreachable." Neither sentence was true. The function never contacted the agent — there was no endpoint to contact — and it read two hardcoded local paths in the *opposite* preference order from the one bun itself uses, so an operator with a custom `[storage] logs` path got "no log store found" from a machine with gigabytes of logs on it. When a doc comment and its function disagree, you have two choices: fix the doc, or make the doc true. We made it true, because the described design was simply better: a `POST /v1/logs/export` endpoint on the agent runs the export server-side against the store's real directory with the Bun-owned checkpoint, the destination (`s3://`, `gs://`, or a path) resolves with the agent's credentials, and the CLI calls it when the agent answers a health probe — falling back to the direct local read, now in the agent's own path order, only when nothing is listening. The endpoint is Admin-only: it writes files wherever you point it, with the agent's credentials, which is not a thing a Deployer token should be able to do.

One more, quieter fix rode along: a clean shutdown used to drop whatever the flush loops had buffered since their last tick. The stop path now forces a final flush of both the metrics and log buffers after the workers have joined, so the last minute survives a restart. And we deleted the dead `KetchupStore` — a second, older log store that Bun constructed and never used, whose calendar/index code and `logs.max_file_size_mb` setting drove nothing. `LogStore` is the live path; the dead one is gone.

## Following the whole cluster

`relish logs web` has asked every node for years. `relish logs web -f` didn't:
it followed the node you happened to be talking to. On a laptop cluster that's
node 1, and the scheduler had just put two of your three replicas on nodes 2
and 3. You'd watch one replica and wonder why the others were so quiet.

We had two places to fix it. The CLI could open one stream per node, or the
node could do it and hand the CLI one merged stream. The laptop settled it:
behind Lima's user-mode network, the host can reach node 1's forwarded port and
nothing else. So the node does the fan-out. When a follow arrives without
`local=true`, the node starts a background task that reads the app's
placements from the council every two seconds, opens a stream to each placed
node (with `local=true&label=true`, so the peer doesn't fan out again and
stamps each line `[node instance]`), and pushes everything into one channel
that feeds the client's server-sent-event response.

The task keeps a map from node name to the `AbortHandle` of that node's
stream. `tokio::spawn` gives you a `JoinHandle`; calling `.abort_handle()` on
it gives you something smaller that can cancel the task but can't wait for
it, which is all a supervisor needs. The loop itself waits on three things at
once:

```rust
tokio::select! {
    () = events.closed() => break,
    Some(ended) = ended_rx.recv() => { /* forget it, warn if it failed */ }
    () = tokio::time::sleep(LOG_FOLLOW_REFRESH) => {}
}
```

`select!` polls every branch and runs whichever finishes first, dropping the
others. Go programmers will recognise `select` on channels; the difference is
that Rust's version works on any future, including a timer and "the client
went away" (`Sender::closed`), not just channel operations. When the client
disconnects, the first branch wins and the whole fan-out winds down.

A node that goes away gets exactly one `warning` event, whichever way it
went: a broken connection, a clean end during a graceful shutdown, or a
stream still hanging on a dead TCP connection that only the membership table
knows is gone. The CLI prints warnings to stderr and lines to stdout, so
`relish logs web -f | grep ERROR` still works while a node dies.

Both ends of that pipe parse SSE, so the parsing lives in one small type,
`ketchup::sse::SseDecoder`. It's incremental on purpose: a network read can
end anywhere, including halfway through a multi-byte UTF-8 character, so the
decoder buffers bytes and only decodes a block once it has seen the blank line
that ends it.

A follow has no natural end, which the CLI's HTTP client didn't know until
0.3.0. `BunClient` gave every request a 300-second deadline with reqwest's
`ClientBuilder::timeout`, and that deadline covers reading the body, not
only waiting for the headers. So `relish logs web -f` on a quiet app died
after five minutes with "request timed out", and so did `relish apply` when
a rolling deploy's progress stream ran longer. The client now carries two
reqwest clients. Ordinary requests keep the 300-second total. Streaming ones
(`logs -f`, the apply and rollback streams) go through a client with no
deadline of its own, and the code applies three. The wait for the headers
gets the ordinary 300 seconds, because the agent binds images before it
starts an apply's stream. After that, a small `StreamBody` type reads one
chunk at a time inside `tokio::time::timeout`, so 60 seconds of silence ends
the command. Silence alone can't mean "dead", since a quiet app writes
nothing for hours, so every SSE route now sends axum's
`KeepAlive::default()`, a comment line (`:`) every 15 seconds that SSE
parsers skip.

Keep-alives cut the other way too: a deploy wedged on the agent would now
hold `relish apply` open for ever, and a soak script with it. So an apply's
or rollback's stream also has a 30-minute deadline, after which the command
says it stopped following and the deploy may still be running. A followed
log has no such deadline; following is what you asked for. Why not reqwest's
own `read_timeout` for the silence? Because it also covers the wait for the
headers, where 60 seconds is too short. The tests use a client with
millisecond deadlines: `a_stream_that_keeps_talking_outlives_the_request_timeout`,
`a_silent_stream_times_out`, `the_wait_for_a_stream_to_start_gets_the_request_timeout`
and `a_deploy_stream_stops_at_its_deadline`.

`relish top` had the same blind spot and a subtler trap. The dashboard already
charted `process_cpu_percent` and `process_memory_bytes`, labelled by app and
PID. Why not have the CLI query those metrics and match them to instances by
PID? Because a PID only means something on the node that issued it, and three
VMs booted from the same image hand out very similar PIDs. So each node joins
its own statuses to its own samples (`bun::top::node_rows`), and the node you
ask merges finished rows. A PID never crosses a machine boundary without the
node it belongs to.

### One stream, two transports

The fix above reached `relish logs -f` and the dashboard, which both speak
SSE. The terminal UI doesn't. It follows logs over a WebSocket, and that
handler still asked the agent for this node's lines and nothing else. The TUI
even said so, in a grey line above the logs: "live logs from the connected
node". Honest, but not much use when the replica you care about is on node 3.

The merge task already existed, so the WebSocket only needed to read from it.
What stood in the way was the channel's item type. `follow_cluster_logs` pushed
`axum::response::sse::Event`s, which is SSE's wire format, and you can't ask an
`Event` afterwards whether it was a warning. The WebSocket has no `event:`
field to carry that distinction anyway. So the task now speaks a type of our
own, and each transport translates it at the edge:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFrame {
    Line(String),
    Warning(String),
}
```

This is a Rust enum whose variants carry data, the thing C needs a tagged
union and a `switch` for and Go approximates with an interface and a type
switch. A `LogFrame` is either a line *or* a warning, never both and never
neither, and `match` won't compile until you've said what happens to each. By
default serde writes an enum like this "externally tagged": the variant name
becomes the JSON key, so a WebSocket frame reads
`{"line":"[node-2 web-1] ready"}` or `{"warning":"node node-3 left the
cluster; no longer following its logs"}`. The SSE side maps `Warning` to an
`event: warning` block, exactly as before, so `relish logs -f` didn't change.

The TUI decodes each frame and shows a warning in line, where it happened,
instead of in a status bar that the next refresh would overwrite. A frame that
doesn't parse is shown as a raw line rather than dropped. A log line shown
oddly beats a log line lost.

Peers relay through the same type. A peer's SSE stream comes back through
`SseDecoder`; a block marked `event: warning` becomes `LogFrame::Warning`, and
anything else becomes a `Line`. There's no third case to forget.

The cluster test,
`tui_logs_history_and_events_cover_every_node_and_name_a_missing_one` in
`tests/placement.rs`, opens the WebSocket the TUI uses through one node of
three, waits for lines stamped with every node's name, shuts a follower down
and waits for the warning that names it.

## Tails that tell the truth

The V02 soak runs a tiny app that appends 1, 2, 3, … to a file ten times a
second and logs `ACK n` after each append. Every few minutes the harness runs
`relish logs soak-writer --tail 20` and saves the answer. The file on disk was
always gapless. The tail wasn't:

```
ACK 967
ACK 968
ACK 974
ACK 969
ACK 975
ACK 970
```

After a node's Bun was SIGKILLed, or its VM powered off, it got stranger:
lines from an hour earlier showed up as the newest. And a CI run showed the
Redis client counting up in twos. Three symptoms, three causes, and none of
them in the app.

**One-second timestamps with no tie-breaker.** `LogStore` stamped each line
with `as_secs()` and queries said `ORDER BY timestamp`. At ten lines a second,
ten rows share every timestamp. SQL doesn't promise any order among equal
keys, and DataFusion reads the Parquet files and the in-memory buffer as
separate partitions and merges them, so rows from two flushes that landed in
the same second came back interleaved. That's the `968, 974, 969, 975`
pattern, and a unit test that writes 200 lines inside one second, flushing
every nine, reproduced it exactly.

**`LIMIT` without `DESC`.** The same query appended `LIMIT n` for a tail. `ORDER
BY timestamp LIMIT 20` is the *first* twenty rows. A cluster query hid this by
fetching everything and trimming after the merge, but a single node answered
`--tail 20` with the oldest twenty lines it had.

**Re-reading capture files from the start.** When Bun restarts, it adopts
running containers and starts a log forwarder for each. The runtime's
`follow_logs` began at byte zero, so the store received the container's whole
history again and stamped every line with *now*. Hour-old `ACK`s became the
newest rows. The Redis client's "stepping by two" was the same thing: an old
instance from a rolling deploy, which had shared the counter with its
replacement and so only ever saw every other value, got re-ingested wholesale
and filled the tail.

The fix gives every row an identity that means something.

A new `sequence` column holds the ingest time in nanoseconds, bumped so it
rises strictly on each node:

```rust
let sequence = nanos.max(self.ingested.last_sequence.saturating_add(1));
self.ingested.last_sequence = sequence;
```

`saturating_add` is Rust's way of saying "add, but stick at `u64::MAX`
instead of wrapping". Plain `+` on integers panics on overflow in a debug
build and wraps silently in release, so for a value that must only ever grow
we pick the behaviour explicitly. Queries now `ORDER BY sequence`, and a tail
is a subquery that takes the newest N and puts them back in order:

```sql
SELECT * FROM (... ORDER BY sequence DESC LIMIT 20) AS tailed
ORDER BY sequence
```

Why nanoseconds and not a plain counter? A counter orders one node's rows
perfectly and says nothing about two nodes. Nanoseconds order each node
exactly and interleave nodes as well as their clocks agree, which is the most
anyone can promise without a shared clock. The cross-node merge sorts on
`(sequence, node)`, so an exact tie still renders the same way every time,
and it deduplicates on `(node, sequence)`. The earlier dedup key,
`(node, timestamp, stream, line)`, had its own quiet bug: an app that printed
the same line twice in one second lost one of them.

The forwarder problem needed the runtime's help. A capture file is
append-only, so the byte offset just past a line's newline names that line
for good. A new `grill::capture::CaptureReader` turns raw chunks into lines
and remembers that offset for each one, counting raw bytes so a line of
invalid UTF-8 (which becomes `U+FFFD` in the `String`) doesn't shift the
positions after it. `follow_logs` now sends a `CapturedLine` carrying the
stream and that position, which also means stderr is finally labelled as
stderr instead of everything being called stdout.

The runc runtime reads stdout and stderr through one reader each, and opens
each where the store's checkpoint left off (more on that below):

```rust
let mut readers = [
    CaptureReader::resume(LogStream::Stdout, stem.with_extension("stdout"), resume).await,
    CaptureReader::resume(LogStream::Stderr, stem.with_extension("stderr"), resume).await,
];
```

The process runtime had the same two files all along (its owner hands
the workload a `.stdout` and a `.stderr` descriptor) but followed only the
first, so a process's stderr was written to disk and never reached the store.
Its `follow_logs` now keeps one reader per stream as well, over the files or,
for a process started without a log directory, over its two in-memory pipe
buffers, and `logs()` answers with stderr after stdout.
`follow_logs_carries_stderr_as_stderr` runs `echo to-out; echo to-err >&2`
in both modes and checks each line's stream. Across the two streams the order
is the order the lines were read in: neither file stamps its lines, so a
stdout line and a stderr line written within the same poll can swap places.
Within one stream, the order is exact.

A file follower can also reach EOF before the process has exited: the child may write its last bytes between the empty read and the next state check. After confirming that the child stopped, the follower scans both files once more before declaring EOF. It preserves the existing resume offsets, so that final scan adds only unseen bytes. Two tests hold the follower at the earlier empty read, release the real child to write stdout and stderr and exit, then check the final lines and their byte positions, including a resumed capture.

A process exiting and its pipe readers reaching EOF are separate events. The in-memory runtime now owns both reader tasks and tracks their completion. A final snapshot waits for that completion outside the process-map lock, and a follower scans once more after completion before returning. `Stopping` is not EOF. This prevents a short-lived child from being reported stopped while its final stdout or stderr chunk is still waiting to reach the buffers.

The first observation of actual child exit starts one two-second drain deadline for both pipes. That deadline belongs to the runtime entry, so canceling a logs request cannot cancel it. A descendant that keeps inherited pipe ends open can delay EOF indefinitely; after the bound, the runtime aborts the remaining readers, reports truncation explicitly in its diagnostic output and preserves bytes already captured. An abort requests cancellation; it does not prove the task has stopped. Each reader owns a completion guard whose destructor confirms that no more bytes can be published, and final readers wait for those acknowledgements. Dropping the last in-memory owner also aborts its readers. File-backed production capture keeps its existing file ownership path. Tests gate final chunk publication after confirmed OS exit, then check snapshots, followers, cancelled waiters, owner drop and a descendant holding the pipes open. A separate gate pauses task destruction after the drain deadline: the final snapshot must remain pending until the reader actually acknowledges termination.

That's a fixed-size array, `[CaptureReader; 2]`, built in place with no
`Vec` and no heap allocation. Our first version built it with `.map` over
an array of `(stream, extension)` pairs, which read nicely. Then opening a
reader had to ask the filesystem how long the file is, and a closure passed
to `map` can't `.await`. Rust has no async `map` on arrays (the closure
would return a future, and you'd get an array of unstarted futures back), so
we wrote the two elements out.

The store then does the bookkeeping. The reader observes device and inode
through the same opened file descriptor that supplies the bytes. Each emitted
capture position carries that identity. A pathname can be replaced between
reading a line and saving its checkpoint; asking the pathname for its identity
later would attach the old offset to the replacement and skip its first lines.

`CaptureReader::read_chunk` keeps its work local through every `.await`. Only
a completed read updates the reader's identity and partial-line state. Cancelling
a read therefore leaves the previous state available for a retry. A replacement
is read from byte zero and never completes a partial line left by the old file.
Resume offsets also carry identity, which is checked again against the opened
file. This closes replacement between loading the checkpoint and starting the
forwarder. Both the process runtime and Runc use that reader path.

The store skips a capture position only when its identity matches the stored
identity and its offset is at or below the stored offset. A caller without a
verified identity can still supply a queryable line; it cannot advance a durable
capture checkpoint.

On each flush the store freezes the offsets, identities and last sequence in
its pending batch. It saves that checkpoint in the Parquet footer alongside the
rows, then publishes the file and replaces `ingest-checkpoint.json`. If the
second replace fails, a restarted store reads the latest footer checkpoint.
There is no separate transaction to complete before recognising those stored
rows. Files written by earlier versions without this metadata remain readable;
they rely on their original JSON checkpoint.

The writer syncs the Parquet file, each newly created containing directory
entry and the publication directory before acknowledging success. Directory-sync errors stay
visible and leave the batch owned for retry. Controlled operation failures and restart fixtures exercise this ordering; it does not substitute for
qualification on a disposable host that actually loses power.

The file identity and offset govern adoption. A capture opened with the
checkpointed identity resumes past the stored lines; a replacement starts at
byte zero, even when it has the same pathname and is longer than the old file.
Each Runc generation has its own capture files, and the process runtime appends
to the capture owned by that execution. The Apple runtime is the exception: `container
logs --follow` hands us lines with no offsets, so an adopted Apple container
is still ingested again after a restart. It's a laptop runtime and the
comment in `apple.rs` says so.

Last, the old-and-new-instance overlap is real during a rolling deploy, so
hiding it would be wrong. Each row now records the `instance` that wrote it,
and when a tail spans more than one instance `relish logs` prefixes each line
with `[instance]`, the way `relish logs -f` already did. Two clients
incrementing one counter now look like two clients.

What about the Parquet files a node wrote before `sequence` existed? Our
first cut made the column nullable so they'd still read, with `NULLS FIRST`
to sort them before everything new. Then we deleted it. At that point, before 0.1.0, we did not carry old formats
forward; we bumped the generation and started a fresh
cluster. That was the pre-release sequence introduction, not the current
post-release migration policy; Chapter 14 describes the release format bumps.
The logs table is durable state and
the `/v1/logs/entries` answer is a node-to-node wire format, so both moved:
protocol 24 to 25, state 40 to 41. With that change, an incompatible node refused to
start at its state stamp instead of quietly half-reading old log files, and
`sequence` is a required column. Only `instance` stays nullable, because
the node's own startup lines don't come from any instance.

The checkpoint itself goes through the same `atomic_write` helper the
identity code uses: a uniquely named temp file, `fsync`, rename over the old
checkpoint, `fsync` the directory. `flush_replaces_the_checkpoint_atomically`
checks that two flushes leave exactly one complete checkpoint and no temp
files behind.

### Resume, don't replay

The checkpoint stopped duplicates. It didn't stop the work. After every Bun
restart each forwarder still opened its capture files at byte 0 and read
the lot, and the store threw away every line at or below its offset. On
the soak's log spammer that was a million lines read, split and discarded,
per restart, per instance. Issue #308 wanted the obvious thing: start at
the offset.

So the offsets travel the other way now. The binary opens the log store
before the agent adopts anything, takes a copy of its offsets, and hands
it to the agent next to the log channel:

```rust
let capture_offsets = log_store.read().await.capture_offsets();
agent.set_log_sink(log_tx, capture_offsets);
```

`follow_logs` gained a `resume: &CaptureOffsets` argument, and
`CaptureReader::resume` starts the reader's byte count at the file's
offset instead of zero. The agent keeps the copy in an `Arc` (a
reference-counted pointer: cloning it bumps a counter rather than copying
the map), so a hundred adopted instances share one map.

An offset is only a promise about one particular file, though. Two things
break it. Truncate the file and the offset points past the end, or into the
middle of a line that isn't the one we counted. Rotate it (rename the old
one away and start a new one under the same name) and the new file may well
grow past the offset before Bun comes back, so length alone can't tell.
The checkpoint now records each capture file's device and inode when it's
saved, and `IngestCheckpoint::load` keeps an offset only while the path is
still the same file and at least that long. Anything else is read from
byte 0, and because the store dropped its offset too, every line in it is
stored. The reader checks the length once more when it opens, which
catches a truncation between the store loading and the forwarder
starting. Recording the inode changed the checkpoint's format, so the
durable state generation moved from 44 to 45.

Two tests pin it. `a_restarted_forwarder_reads_only_the_lines_past_the_checkpoint`
in `grill/process.rs` ingests three lines, flushes, appends two more and
opens a fresh store and forwarder: the forwarder must *read* exactly the two
new lines, not merely store two, and after truncating the file it must read
the one line that's left. `a_rotated_capture_file_is_read_again_from_the_start`
renames the capture file away and writes a longer one in its place, which is
the case a length check alone gets wrong.

`load` was also the only place that forgot a capture. Every singleton job
that printed added its capture file's path to the checkpoint, the job's
instance deleted the file when it retired, and the entry stayed until Bun
restarted. Each flush cloned the whole map, wrote it into the Parquet footer
and wrote it again as JSON, so a node running a printing cron job every
minute carried one more entry a minute, three times over (#1417). Now each
flush runs the same check on the blocking pool before it writes: a path that
is gone, or is a different file now, leaves the copy being written, and once
the flush is durable the store forgets it too, unless the path has meanwhile
been ingested again as a new file. The test
`the_checkpoint_forgets_captures_whose_files_are_gone` ingests from two
hundred job captures, deletes them and checks that one flush leaves only the
capture still on disk, in memory and in the saved checkpoint.

### Ask everyone, not just the current home

All of that shipped, and the next soak still caught a stale tail. Right
after a graceful stop and start of the whole cluster, the Redis client's
tail read `INCR 749 … 782`, counting in twos, while the counter was past
2,500. Forty seconds later the tail was right again.

We went looking for another re-ingestion bug and built tests for every way
one could happen: a graceful restart with a retired instance's capture file
still on disk, a graceful stop with lines only in the buffer. Both passed.
The store was fine. The status snapshots told the real story. The client had
started on node 1, moved to node 3 during a test run, then to node 2, where
it spent half an hour. After the restart the scheduler put it back on node 1.

And the cluster-wide query only asked the nodes where the app was placed
*now*. Node 1's newest stored lines for the app were from the moment it left,
thirty minutes earlier, during a rolling overlap (hence the twos). Node 2,
which held everything since, was never asked. Nothing was out of order. We
were just asking the wrong nodes.

Lines live where they were produced, and they stay there when the app moves.
Placement records where an app runs, not where it ran. So the fan-out now
asks every live member, and a node that never ran the app answers with an
empty list:

```rust
pub fn query_targets(placed: &[String], members: &[(String, String)]) -> QueryTargets {
    QueryTargets {
        reachable: members.to_vec(),
        unreachable: placed
            .iter()
            .filter(|node| !members.iter().any(|(id, _)| id == *node))
            .cloned()
            .collect(),
    }
}
```

`members.to_vec()` copies the borrowed slice into an owned `Vec`, which needs
the element type to be `Clone`; `(String, String)` is, because both halves
are. Placement still matters for one thing: a placed node gossip no longer
lists is certainly holding lines we want, so it comes back as a warning
rather than silently shrinking the answer.

Is asking every node expensive? Each node answers with only its own tail and
the query is interactive, so on a laptop's three nodes it costs nothing. At
the ten thousand nodes we design for it's a real cost, and one we'll watch.
The alternative, recording
every node an app ever ran on, is state the cluster would have to keep
forever for the sake of a log query. `tail_after_an_app_moves_back_includes_the_nodes_it_ran_on_meanwhile`
reproduces the soak with two stores and fails against the old node choice.

What we could bound straight away was how many nodes one query asks at once
(#2453). `fan_out_query` spawned a task per node into a `JoinSet` and let them
all send, so a 500-node cluster opened 500 requests for one `relish logs`.
Each task now takes a permit from a `tokio::sync::Semaphore` holding
`MAX_CONCURRENT_NODE_QUERIES` (16) before it sends:

```rust
let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_NODE_QUERIES));
// ...in each task:
let _permit = slots.acquire_owned().await.ok();
```

A semaphore is a counter of permits: `acquire` waits until one is free and
hands back a guard, and dropping the guard returns the permit. Like a
`MutexGuard`, the permit's lifetime is the critical section, so there's no
"release" call to forget; the task ends, the permit drops. `acquire_owned`
needs the semaphore in an `Arc` because the permit it returns owns a
reference to it, and a spawned task can't borrow from the function that
spawned it. The leading underscore in `_permit` keeps the guard alive to the
end of the task without a "never used" warning; a bare `_` would drop it at
once. The per-node deadline starts after the permit, so a node queued behind
slow ones isn't reported as timed out. The handler also stopped cloning the
whole desired state to find the app's placements: `read_desired` takes a
closure, runs it under the read lock, and returns only the node ids.
`a_fan_out_runs_at_most_the_concurrency_limit_at_once` counts the requests a
test server has in flight and fails at 64 without the bound.

### Two runs, one name

The final soak tier found the next one, and it was the same shape again. The
tail read `INCR 11630`, `INCR 11632`, `INCR 11631`, `INCR 11633`: one swap,
no label, so it looked like a single client counting backwards. The status
snapshots either side of it had the client on node 2 and then on node 3. A
rollback walk had just restarted every node's Bun, and the scheduler moved
the client in the middle of it. Same instance name, `default__soak-redis-client-0`,
on both nodes.

The labelling we added for rolling deploys keys on the instance name, so two
runs of one name printed bare. And the merge put them in order by `sequence`,
which is ingest time in nanoseconds *on the node that stored the line*. Each
node's sequence rises strictly, so node 2's lines are in order and so are
node 3's. Between the two, the order is only as good as two VMs' clocks plus
up to 200 ms of capture polling each. The old run's last `INCR` and the new
run's first one were well inside that. Nothing went backwards. Two processes
wrote, and we sorted them by two clocks.

Could we sort them properly? Not with timestamps. The capture file holds raw
bytes with no emission time, and even an emission time would come from two
different clocks. There is no single order between two processes on two
machines to recover, only the counter itself. So the fix is to stop hiding
that there were two. The cross-node merge now names the node on every entry:

```rust
let entry = LogEntry {
    node: Some(source.node_id.clone()),
    ..entry
};
```

`..entry` is the struct update syntax from Chapter 1. Here it *moves* the
other fields out of `entry`, strings and all, with no copying; that's fine
because the loop owns `entry` and never touches it again. The field itself is `#[serde(default,
skip_serializing_if = "Option::is_none")]`, so a node answering for itself
sends exactly what it sent before, and the node-to-node format doesn't move.

`relish logs` then labels by *run*, an `(instance, node)` pair, and adds the
node only where the name alone is ambiguous: `[default__soak-redis-client-0@rb-2]`.
Two replicas on two nodes keep their short `[instance]` labels. The soak
checker now judges order within each labelled run, and "the view ends below
what we saw before" against the newest line of any run.
`one_instance_on_two_nodes_names_each_line_s_node` and
`merged_entries_name_the_node_that_stored_them` fail without the change.

### Eleven minutes of splitting lines

Re-reading every capture file from byte 0 after a restart looked free: the
store skips what it already holds. Then a release candidate's soak SIGKILLed
the leader's Bun, systemd started a new one two seconds later, and it
printed nothing for eleven minutes. At 21:00:25 it gave up with `cannot
restore workload ownership: runtime adoption timed out for
default__hello-0` and exited. systemd's summary line had the real clue:
`Consumed 11min 24.238s CPU time` in 11 minutes 17 seconds of wall time.
Something had kept a CPU busy the whole time while adoption waited.

Adoption itself was innocent. As it adopts each instance, Bun spawns the
log forwarder that follows its capture file from the start. One of the
soak's workloads is a log spammer, 200 lines a second, and after 80 minutes
its capture held about a million lines. The forwarder read the whole file
into one buffer and handed it to `CaptureReader::push`, which did this:

```rust
while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
    let raw: Vec<u8> = self.partial.drain(..=newline).collect();
    // ...
}
```

`Vec::drain(..=newline)` removes the front of the vector, and a `Vec` keeps
its elements contiguous, so every call shifts all the bytes behind it
forward. One line costs the size of the rest of the buffer; a million lines
cost a million times half of 56 MB. It's the same trap as `list.pop(0)` in
a Python loop, and like the Python version it's invisible on the small
inputs every unit test uses.

That's the slow part. What made it fatal is where it ran. Tokio runs tasks
on a small pool of worker threads, one per CPU by default, and a task only
gives its thread back at an `.await`. `push` is an ordinary synchronous
function, so for as long as it ran, its worker ran nothing else. The soak
VMs have two vCPUs, so two workers. Tokio can steal queued work from a busy
worker, but not all of it: the task a worker has just woken waits in a slot
only that worker polls. The journals can't tell us exactly which task was
stuck where, only that adoption's work and its 10 s deadline (a timer needs
a free worker to notice it) got nowhere until the splitting was done. By
then the deadline was eleven minutes old. The numbers fit: the unit test
below, a 4 MiB backlog, stalls for 5.5 s with the old code, and the cost
grows with bytes times lines, which puts the spammer's capture at around
ten minutes.

Why did earlier candidates survive the same fault? Scheduling and log
volume. Two seconds after this failure, systemd's next Bun adopted all nine
instances with the same files on disk, because this time the scheduling fell
the other way. The bug arrived with the offsets change a few days earlier,
and it needs a capture this big on the node being killed.

The fix has two halves. `push` now searches only the bytes it was just
given, and copies each of them at most once:

```rust
let mut rest = bytes;
loop {
    // ... skip to the next newline while `discarding` ...
    let room = MAX_CAPTURED_LINE_BYTES.saturating_sub(self.partial.len());
    let Some(newline) = rest.iter().position(|byte| *byte == b'\n') else {
        // ... keep `rest` for the next chunk, up to `room` bytes ...
        return lines;
    };
    let kept = newline.min(room);
    self.partial.extend_from_slice(&rest[..kept]);
    lines.push(self.take_line(newline - kept + 1));
    rest = &rest[newline + 1..];
}
```

`rest[newline + 1..]` is a slice, a borrowed view into the caller's buffer
with no copy, like a Go slice expression. Moving `rest` forward costs
nothing; only the bytes of a line get copied, once, into `partial`, which
`take_line` clears without giving back its memory. Searching only the new
bytes is safe because bytes already waiting are, by definition, the part
of a line with no newline yet.

The second half matters more: no forwarder reads a whole file any more.
`read_capture_chunk` returns at most `CAPTURE_CHUNK_BYTES` (64 KiB) from a
given offset, and both the runc and process runtimes loop on it, skipping
their 200 ms poll interval while chunks come back full. Every chunk is a
`tokio::fs` read, which is an `.await`, so between chunks the worker is free
for everyone else. A hundred-megabyte backlog still takes a while to
replay; it just can't take the node with it. (Since "Resume, don't
replay" above, a restart only replays a capture the store has no usable
offset for, so the backlog is usually the few seconds Bun was down.) The process runtime had a
quieter version of the same problem: it called `std::fs::read` on the whole
file every 200 ms, blocking I/O on a runtime thread, for as long as the
workload lived.

The tests measure the thing the soak felt. `replaying_a_large_capture_backlog_does_not_hold_the_runtime`
writes a 4 MiB backlog, follows it on a single-threaded runtime, and times a
1 ms sleep in a loop while the lines arrive; the old code held the runtime
for 5.5 s in one go, the new one for a few milliseconds at most. The runc version,
`following_a_large_capture_after_a_restart_does_not_hold_the_runtime`, has a
real container print 200,000 lines and follows it from a fresh runtime
handle, as a restarted Bun would. `a_backlog_of_short_lines_splits_in_linear_time`
pins `push` itself.

What's `room` for? A line has to end somewhere. Output with no newline, a
`\r` progress bar or a binary dump, would otherwise sit in `partial` in full
until the newline finally came. So a line stops growing at `MAX_CAPTURED_LINE_BYTES` (64 KiB).
The reader hands on that prefix as the line and sets `discarding`, the flag
it already used to skip the rest of a line whose start was reclaimed, so the
remainder is counted and dropped up to the next newline.

Counting isn't enough on its own, though. The log store learns how far a
capture has been read only from the positions on the lines it ingests, and
it only reclaims a capture's disk up to that offset. Skipped bytes have no
line, so a progress bar that never printed a newline kept its whole capture
file on disk, growing forever, while memory stayed flat. So every
`SKIPPED_REPORT_BYTES` (1 MiB) of skipping, the reader hands on a small line
of its own, `[skipped 1048576 more bytes of a cut line]`, carrying the
position it has reached. The store needs no new idea: it ingests a line,
moves the offset and reclaims behind it, exactly as before. Why not a
record with a position and no line? We considered it. The store saves its
offsets only when it flushes rows, so an offset with no row would wait
for some other line to come along, and on a quiet node that might never
happen. The marker costs one short row per megabyte, and it also tells you
the line went on. `a_capture_with_no_newline_is_reclaimed_once_flushed`
follows an 8 MiB capture with no newline into a real `LogStore`, flushes it
and checks that the file's disk allocation drops to the retained tail.
`a_stream_with_no_newline_stays_within_the_line_cap` pushes 100 MiB with no
newline in 64 KiB chunks and checks the buffer after every one.

Those markers have a side effect a reviewer caught. They checkpoint an offset
in the middle of a line, and a reader resumed after a Bun restart always
assumed its offset started a line. So the rest of a never-ending line came
out looking like a fresh one. `resume` now looks at the byte before the
offset. If it isn't a newline (a reclaimed hole reads as zeros, which count
as mid-line too), the reader starts out skipping, just as it would have
without the restart. `resume_inside_a_skipped_line_keeps_skipping_it` and
`resume_after_a_zeroed_byte_keeps_skipping` pin both cases.

A cut line has to say so, or a reader takes 64 KiB for the whole line. So
it ends with ` [line cut at 64 KiB]`. And the cut must not land inside a
character. Rust's `String` is always valid UTF-8, so `from_utf8_lossy`
turns half an "é" into U+FFFD, the replacement character, and the line
would end in garbage the workload never wrote. UTF-8 makes the fix cheap:
every byte after the first one of a multi-byte character starts with the
bits `10`. If the first byte left out is one of those, the character
began inside the kept part, so `character_boundary` steps back to the
byte that starts it, at most three bytes. In Go you'd reach for
`utf8.RuneStart`; in Rust it's one mask, `byte & 0b1100_0000 ==
0b1000_0000`. `a_line_is_cut_at_a_character_boundary` puts the cap in the
middle of an "é" and checks that the kept text is a clean prefix.

Should one stuck instance be able to stop a node at all? Adoption is
fail-closed by design: an instance Bun can't prove it owns stops startup,
because the alternative is a reconciler that doesn't see it and starts a
second copy under the same name, port and address. Bounding each adoption
and quarantining the stuck one is possible, but the quarantined instance
would have to keep its name, port, lease and discovery address fenced, stay
out of both "running" and "gone" in reports, and be retried. That's a
design change with its own tests, and we've written it down for after
0.1.0 rather than slip it in behind this fix.

### Giving the disk back

0.2.3's day run ran three nodes for 24 hours, and near the end node 1's
disk crossed 80%. The soak's inventory blamed `instances/`, the directory
that holds instance records and capture files: 949 MiB on node 1, against
32 MiB and 24 MiB on the other two. We filed it as job instance storage,
since the run had just started 194,000 fresh `runc` jobs.

The numbers said otherwise. Plotted against time, node 1's `instances/` grew
in a straight line, 26.4 MiB every 40 minutes, about 39.6 MiB an hour. Job
load doesn't draw straight lines. The soak's log spammer does: twenty lines
of about 57 bytes every tenth of a second, close to 11 KB a second, or about
39 MiB an hour. It lived on node 1 for the whole run. Its capture file held every
line it had ever printed, and so did the log store, which caps itself and
prunes. Only the store's copy was bounded.

Why not rotate the capture, the way `logrotate` would? The app writes into
the file directly, through a descriptor its owner process opened with
`O_APPEND`, so Bun can't swap the file under it without the app's
cooperation. Truncating it in place loses whatever the app writes between
our last read and the truncate. Worse, either one changes what an offset
means, and the whole of "Resume, don't replay" rests on an offset naming one
line for good.

So we keep the offsets and drop the bytes. Linux's `fallocate` with
`FALLOC_FL_PUNCH_HOLE` frees the blocks behind a byte range and leaves a
*hole*: the range reads back as zeros, takes no disk, and the file keeps its
length. `FALLOC_FL_KEEP_SIZE` makes that explicit. Writes with `O_APPEND`
carry on at the end as if nothing happened. macOS has the same thing as an
`fcntl` called `F_PUNCHHOLE`. `ls -l` still shows the file's full length;
`du` shows what it takes.

The new function lives in `grill/capture.rs`:

```rust
pub fn reclaim_ingested(
    path: &Path,
    identity: CaptureFileIdentity,
    ingested_through: u64,
) -> io::Result<u64> {
    // ... open for writing, refuse a symlink, check the identity ...
    let block = metadata.blksize().max(1);
    let end = ingested_through
        .saturating_sub(1)
        .min(metadata.len().saturating_sub(RETAINED_CAPTURE_BYTES));
    let end = end - end % block;
    let start = data_start(&file, 0)?;
    let start = start - start % block;
    if start < end {
        punch_hole(&file, start, end - start)?;
    }
    Ok(end.max(start))
}
```

Three rules hide in those lines. It never frees a byte the store hasn't
ingested (`ingested_through`). It keeps the last mebibyte
(`RETAINED_CAPTURE_BYTES`), which is exactly the window a tail snapshot
reads, so `relish logs -f` still starts from real lines. And it only frees
whole filesystem blocks: a punch that ends mid-block zeroes the partial block
without freeing it, which would only cost a write.

The `saturating_sub(1)` is there for a checkpoint that lands exactly on a
block boundary. A store that restarts resumes at its checkpoint and reads the
byte before it: a newline means the checkpoint starts a line, anything else
means it's partway through one, and the reader skips to the next newline. A
hole reads as zeros, so a punch that ended exactly at the checkpoint would
turn that newline into a zero, and the resume would throw away a whole line
it never stored. Rounding down from one byte earlier keeps the byte before
any checkpoint on disk. `a_resume_at_a_block_aligned_checkpoint_after_reclaiming_keeps_its_first_line`
reclaims up to a checkpoint at 4 MiB and resumes there.

Who calls it, and when? Not the forwarder: a line it has read may still be
sitting in the store's memory buffer, and a crash would lose it. The store's
flush calls it, on the blocking pool, after the Parquet file and the
checkpoint are both durable. Only files the checkpoint has read more than a
mebibyte of qualify, so a flush touches a handful of chatty captures, not the
thousands of small job ones. A failure is printed and the flush stands; it
costs disk, not data.

The punch is one call per platform, picked at compile time with
`#[cfg(target_os = "linux")]` and `#[cfg(target_os = "macos")]`. Only the
matching function exists in the binary, the way an `#ifdef` works in C, and a
third version for every other platform reports `Unsupported`. Linux goes
through the `nix` crate's safe wrapper. macOS has none, so it calls `fcntl`
through `libc` in an `unsafe` block, with the comment the project asks for:

```rust
// SAFETY: `file` is an open descriptor for the call's duration, and
// F_PUNCHHOLE reads one `fpunchhole_t`, which `hole` is and outlives the
// call. The kernel does not keep the pointer.
let result = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_PUNCHHOLE, &hole) };
```

`unsafe` doesn't switch the borrow checker off. It lets us do the one thing
the compiler can't check (call a C function that takes a raw pointer), and
the comment says why the pointer stays valid.

Holes also change what a reader can meet. A store that loses its checkpoint
reads the capture from byte 0 again, and byte 0 is now in a hole. Handing the
store a megabyte of zeros as one line would be worse than useless. So
`CaptureReader::read_chunk` asks the kernel where the data starts with
`lseek(SEEK_DATA)`, jumps there, and drops everything up to the next newline,
because the line the hole ends in may have lost its start. Every line in that
stretch is already stored, so dropping one costs nothing. `read_retained`
does the same for the code that reads a whole capture at once.

`data_start` takes its file as `file: &impl std::os::fd::AsFd`. That's
`impl Trait` in argument position: "any type that can lend out a file
descriptor", decided at compile time like a C++ template, not a Go interface
looked up at run time. The reader passes a `tokio::fs::File` and the punch
passes a `std::fs::File`, and both get their own copy of the function.

The tests use real files. `reclaiming_frees_ingested_bytes_but_keeps_the_length_and_identity`
writes 8 MiB of numbered lines, reclaims, and checks the length and inode
stayed while the allocated blocks fell to the last mebibyte;
`reclaiming_never_frees_bytes_the_store_has_not_ingested` and
`reclaiming_a_replaced_file_frees_nothing` hold the other two rules.
`a_reader_from_byte_zero_skips_a_reclaimed_prefix` checks the first line a
fresh reader returns is whole and numbered where it should be.
`a_durable_flush_reclaims_the_ingested_capture_prefix` checks nothing is
freed until the flush, and `a_chatty_instance_s_capture_stays_bounded_once_its_lines_are_stored`
runs a real process that prints 8 MiB through `yes`, follows it into a store,
flushes, and measures what the flush freed.

It used to measure what was left, and on a busy Mac it failed now and then
with 1.4 MB still allocated against a bound of 1.25 (#1537). Under load it
failed about once in 25 runs, so we could look. The data was exactly where it
should be: `lseek(SEEK_DATA)` found the first byte at 7 MiB and the last at
8 MiB, so the punch had freed everything it was meant to. The extra blocks
were there *before* the flush, past the end of the file. APFS gives a file
that's being appended to some space ahead of its writer, and under load it
gave up to a megabyte. `st_blocks` counts that space, the writer exiting
didn't return it, and no punch inside the file's length can touch it. The
product was fine; the test was counting something the reclaim doesn't own.
So now the test makes that allocation itself, on every filesystem
(`F_PREALLOCATE` on macOS, `fallocate` with `FALLOC_FL_KEEP_SIZE` on Linux),
and asserts that the flush freed all but the retained tail. The old
assertion fails every time with the preallocation in place; the new one
passed 544 runs in a row under the same load.

What we didn't do: route every app's output through Bun, which would let us
rotate at will but put Bun in the path of every byte every app writes, and
make an app block when Bun is slow. The issue also asked about the soak's own
export directory, which grows about 600 MiB a day on each node. That's the
harness's to bound, and it stays open in #1418.

### Dropped, and saying so

The buffer in front of all this had a cap from the first hardening pass: a
million rows, after which the oldest go. It was meant for a disk that keeps
failing. The 16 November review found it bit a healthy disk too. Flushes ran
once a minute and nothing flushed sooner, so an app printing more than about
17,000 lines a second lost lines (#2057). Nothing said so. `ingest` moves the
capture offset forward when a line arrives, before it's stored, so the next
checkpoint skipped the lost lines, and the reclaim from "Giving the disk
back" then punched holes over the capture's copy. And the cap counted rows,
not bytes: a million 4 KiB lines is 4 GiB.

The first two changes make dropping rare. The store says when it wants
flushing, and the task that feeds it wakes the flush task:

```rust
let flush_wanted = {
    let mut store = drain_store.write().await;
    store.ingest(&rec);
    store.flush_wanted()
};
if flush_wanted {
    flush_now.notify_one();
}
```

The block computes a value and drops the write lock at its closing brace, so
the notification goes out with no lock held. `tokio::sync::Notify` is a
wake-up with no payload. If nobody is waiting, `notify_one` stores a single
permit, so a burst of calls while a flush is running becomes one more flush,
not a queue of them. The flush task waits on `notified()` beside its minute
tick, as a `tokio::select!` branch written `_ = log_flush_now.notified(), if
!flush_failing => {}`. The `, if` is the macro's precondition: while the last
flush failed, that branch is switched off and only the tick retries, so a full
disk doesn't turn every ingested line into another failing flush. At 100,000 rows
or 32 MiB the store asks; the caps are now 1,000,000 rows *or* 256 MiB,
counting a flush still being written.

The third change decides what a drop means. Our first answer was replay:
each buffered line remembered where in its capture it started, and no
checkpoint saved an offset past the first dropped line, so a restart would
read the lost lines again. Review found two problems with it, both visible
in our own test. Capture files don't rotate, so the hold lasted for the rest
of Bun's life: the reclaim never got past it, and a chatty instance's capture
grew without bound, the disk fill of #1418 back again. And a restart re-read
everything from the hold onwards, including the lines already in Parquet, so
the store's promise never to store a line twice was broken.

So a drop is now an honest, counted loss. The checkpoint moves on as usual.
Each log that lost lines gets one marker line, on stderr, where the gap is:

```rust
while self.buffer.len() > row_limit || self.buffer_bytes > byte_limit {
    let Some(entry) = self.buffer.pop_front() else {
        break;
    };
    overflow += 1;
    self.buffer_bytes -= entry.size();
    let owner = (entry.app, entry.namespace, entry.instance);
    let marker = self.drop_markers.entry(owner).or_insert(DropMarker {
        count: 0,
        timestamp: entry.timestamp,
        sequence: entry.sequence,
    });
    marker.count += 1;
    marker.timestamp = entry.timestamp;
    marker.sequence = entry.sequence;
}
```

`pop_front` hands the oldest element over by value, so the loop can move the
strings into the map key instead of copying them. The first version of this
loop used `self.buffer.drain(0..overflow)` on a `Vec`, which reads the same
and hides a cost the audit caught ([#2450](https://github.com/reliaburger/reliaburger/issues/2450)):
removing from the front of a `Vec` shifts every remaining element down to
close the gap. At the cap that's one shed line per ingested line, and each
shed moved a million `BufferedLogEntry`s, about 100 MB of memmove, under the
write lock, exactly when the node was already struggling to keep up. The
metrics buffer did the same per sample (64 MB each), and the rollup buffer
per ingested rollup. All three buffers are now a
`std::collections::VecDeque`, a ring buffer: it keeps its elements in a
circular array with a head and a tail index, so pushing at the back and
popping at the front are both O(1), where a `Vec` is O(n) at the front. The
caps and what they drop didn't change, only what dropping costs.
`the_newest_samples_survive_shedding` and its log and rollup twins fill past
a small cap and check the newest rows are the ones left.

The marker takes the last dropped line's sequence,
so a query sorts it into the gap. Markers live beside the buffer, not in it,
so they're never dropped themselves; the next flush writes them as ordinary
rows reading "3 log lines dropped: the log store fell behind". The store also
counts the lines in `dropped_lines`, which Bun's collection loop reports as
the metric `ketchup_log_lines_dropped_total`, and Bun's log says how many
went at each flush.

`shed_lines_are_counted_and_marked_in_the_instance_log` caps the buffer at
two rows, drops three of seven lines and expects the marker between the
survivors. `a_restart_after_a_shed_stores_nothing_twice` reopens the store
and replays the whole capture into it: nothing is stored again.
`the_capture_is_reclaimed_after_a_shed` follows a 4 MiB capture into a store
capped at 1,000 rows and checks the flush still frees all but the retained
tail. With the replay version it freed nothing.

### The tail that stopped

`relish logs soak-redis-client --tail 20` answered `INCR 1628` on 30 October
2026, 27 minutes into a day-long run. It gave the same answer for the next
21 hours, while the app counted past seven thousand and beyond.
`soak-writer` froze at `ACK 61022` an hour and a half later. No warning, no
error, twenty tidy lines.

Which part of the path would you blame? We blamed the query. A tail is a
`SELECT ... ORDER BY sequence DESC LIMIT n` across a Parquet directory, and
DataFusion's top-k plan shares a filter between partitions and prunes files
with it. Picking the wrong file after a rotation sounded right, and the freeze
times differing by app sounded like a size threshold. So we wrote the test: a
chatty app and a quiet one, forty flushes, row groups splitting every file, a
tail after each flush. It passed. At the soak's real rates for 150 simulated
minutes on four partitions, it passed. It passes today, along with
`tail_crosses_parquet_a_flush_in_flight_and_the_buffer` and
`tail_survives_the_oldest_files_being_pruned`. The query was never wrong.

The run's evidence directory settled it. Every check lists each node's files,
and after 18:49 on the first evening no node wrote another log file:

| Node | Runs | Last Parquet file | Written by |
|---|---|---|---|
| 1 | `soak-redis-client`, `soak-spammer` | `logs_000120` | 18:49 |
| 2 | `soak-writer` | `logs_000120` | 18:49 |
| 3 | `soak-redis` | `logs_000025` | 18:24 |

A flush with an empty buffer writes nothing, so the store wasn't failing to
write. It had nothing to write. And the same run's other mystery fell out: node 1
ended with 858 MiB of instance storage against 9 MiB on node 2. That's the
spammer's capture, which [Giving the disk back](#giving-the-disk-back) can only
reclaim once a flush has stored it. The lines were reaching the capture files
and stopping there.

Here's what stopped them, in the Runc follower:

```rust
terminal = match &context {
    Some(context) => !matches!(
        context.role_state(RuntimeRole::Launcher).await,
        Ok(Some(CommandState::Running { .. }))
    ),
    None => true,
};
```

"Not `Ok(Running)`" lumps two very different answers together. `Ok(Retired)`
is the owner saying the workload is gone. `Err(..)` is us failing to find out.
`role_state` asks the instance's owner process over a Unix socket, the owner
serves one client at a time, and each request gets two seconds. The follower
asks five times a second. On a four-vCPU VM running 23,000 host jobs a second,
it only takes one answer in several hours to miss that deadline. The follower
reads the error as the end of the workload, finishes, and returns. Nothing
starts it again until the instance or Bun restarts. That fits the eight-hour soak never
seeing this: its faults restart Bun all the time, at a sixteenth of the job
rate. Only a
quiet cluster with no faults could keep a dead forwarder dead for a day.

The process runtime had the same mistake in a shorter form, `Err(_) => return`,
which let us reproduce it on any machine.
`a_failed_state_check_keeps_a_running_instance_in_the_log_store` follows a real
capture through four flushes and makes one state check fail. It failed on the
first one, with "the follower ended while its instance ran", and passes now.

The fix is to stop treating "I don't know" as "no". The Runc loop moved into
`capture::follow_capture`, which the compiler lets every platform build and
test, and it takes its check as a closure:

```rust
pub async fn follow_capture<Checked>(
    mut readers: [CaptureReader; 2],
    sender: mpsc::Sender<CapturedLine>,
    limit: Duration,
    mut running: impl FnMut() -> Checked,
) where
    Checked: Future<Output = io::Result<bool>>,
```

If you're coming from Go, the closure is a `func() (bool, error)`. The generic
`Checked` exists because every `async` block has its own anonymous type, so a
function can't spell it out; it can only say "some future that produces an
`io::Result<bool>`". `Ok(false)` ends the follow, `Ok(true)` carries on, and an
`Err` is news of nothing.

Could a follower now run forever? It could, for an instance whose records have
been deleted, because no check will ever answer for it. So a failed check has
two ways out. If the capture files have gone, the instance was cleaned up and
the follower stops at once. Otherwise `SourceWatch` starts a ten-minute clock,
and any answer or any new output stops it. A workload that keeps writing is
never dropped, however broken its owner is. Only one that's silent *and*
unaccountable for ten minutes is let go, and that one leaves a line in Bun's
log, because the old exit said nothing at all and that silence cost us two
releases. The same patience covers the follower's first step, which could
fail the same way and leave an instance out of the store from birth.

Why not restart a dead forwarder from a supervisor? That'd need a second loop
to notice that a task has gone, and still a rule for when not to restart it.
The rule is the hard part, and the follower already has what it needs to apply
it.

One thing still looks odd. `--since 3m` returned current lines, which is what
pointed us at the query. It only looked healthy. The store had
nothing that recent, so `relish logs` fell back to the agent's plain-text
endpoint, which reads the capture file directly and ignores `start`. So on
the node holding the instance, a window of three minutes printed everything
the capture had kept, and the newest line was indeed new.

We never saw the failed check itself: the journal the run kept starts about
19 hours later, and the old exit logged nothing anyway. So the chain is strong
rather than proven. The stores stopped, the captures didn't, and this is the
one way out of the loop while an instance runs. The next long run's `--tail`
is the check that closes it.

The fallback itself was the next fix (#1515). Could the capture endpoint learn
to honour `start`? Not cheaply. A capture file is the raw bytes the workload
wrote, with no time on any line, so the endpoint would have to start stamping
lines it currently copies verbatim. It doesn't need to, though. The store's
query unions its unflushed buffer with the Parquet files, so a line is in the
store within a moment of being written, and the store is the only place that
knows when each line arrived. `relish logs` already refused to fall back for
`--until`, `--stream` and a named instance, for the same reason: an empty
answer from the store is the answer. `--since` now joins them.

Without a window the fallback stays, and it had a second rough edge. The
cross-node query fans out to every node, but the capture endpoint only reads
the node you asked. Ask a node that doesn't run the app, get nothing from the
store, and the fallback's `404 no local capture for this workload` came back
as an error. When the store has already answered for the cluster, that 404
just means "not here", so `relish logs` now prints nothing instead. If the
store didn't answer, the 404 is still the most honest thing to show, and it
still surfaces. The match is a single arm with a guard:

```rust
match self.logs_local(app, namespace, options).await {
    Err(RelishError::ApiError { status: 404, .. }) if store_answered => Ok(String::new()),
    other => other,
}
```

The `..` in the pattern ignores the variant's other fields (the body here),
and the `if` after the pattern is a match guard: the arm only matches when
both the shape and the condition hold. Everything else, success included,
falls through to `other` and is returned unchanged.

`since_on_a_quiet_app_prints_nothing_from_the_old_capture` serves an empty
store answer beside a capture holding old lines and asks with a window. It
printed the old lines before the fix. Its neighbours pin the rest: lines
inside the window still print, an unwindowed query still reads the capture,
and a 404 is only forgiven once the store has spoken.

## When nothing looks like success

Both hardening passes share a pattern, and later reviews kept finding more of it: a failure that comes back dressed as an empty, successful answer. A directory called `blocked.parquet` made the exporter and both retention loops report success. A peer that sent `200 OK` and then went quiet hung a log query. A node that answered `{}` convinced the diagnostic collector there were no alerts. None of these crash. They lie quietly, which is worse.

### Report export failures before the disk fills

The disk-pressure loop already refused to delete content that hadn't been
exported, but it discarded export errors. A broken destination could therefore
leave the disk filling with no explanation. `PressureResult` now carries an
optional export error, and Bun prints it with the affected store's name.

The regression writes a log file, configures an unsupported export destination,
and sets the pressure threshold below the file's size. It asserts both that the
failure is reported and that the local file survives. Reporting a failed backup
mustn't turn it into permission to delete the only copy.

A failed export still holds every file, and that means it holds retention
too: with an `export_path` set, a tick whose export fails prunes nothing, by
size or by age, until an export works again. That's the right call (deleting
the only copy of what we couldn't ship is the bug the checkpoint exists to
prevent), but an operator only found out from a log line every five minutes
while the disk filled (#2062). Bun now records which store's export is
failing and since when, the diagnostics endpoint reports it, and `relish wtf`
turns it into an `export-failing` warning that says retention is on hold. The
record carries no error text, because the diagnostics payload never carries
host paths, and an object store's error can name one; the warning points at
Bun's log for the reason.

### Hash once

The disk-pressure tick exports before it prunes, and each export read every
Parquet file in the store and hashed it twice: once for the durable id, once
more for the object key. Then the pruner hashed each file a third time to
check it had shipped. With the default caps that's about 3 GiB of SHA-256
every five minutes, inline on a runtime worker, for files that never change
once written (#2058). Now one function does the hashing, `file_sha256`, on
the blocking pool, and it remembers each result:

```rust
static HASHES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, (FileStamp, String)>>,
> = std::sync::LazyLock::new(Default::default);
```

A `static` in Rust has to be built at compile time, and a `HashMap`
allocates, so it can't be. `LazyLock` builds it on first use and then hands
out the same one forever, like a function-local `static` in C that's
initialised once. The `Mutex` is the `std` one, not tokio's, because it's
only ever locked for a lookup or an insert, in synchronous code, never across
an `.await`. `FileStamp` is the length, modification time, device and inode,
all taken from one `fstat` of the open file. A flush publishes by renaming a
new file over the name, so a reused name is a new inode and misses the cache.
`an_unchanged_file_is_hashed_once_across_exports_and_prunes` exports three
files twice and asks the pruner about each: before, nine hashes; now, three.

### Offline exports must report partial success

Copying the archive is only half an incremental export. We also need to persist
which files were copied. The offline CLI used to discard a checkpoint write
failure and print success. It now returns an error that says the files arrived
but a later export may repeat them. The `?` operator propagates that error before
we print the success message. A CLI regression makes the checkpoint path a
directory, then checks both the copied bytes and the non-zero exit status.

`relish logs-export --source PATH --dest PATH` explicitly selects a local store,
including a custom store whose agent is stopped. A second regression exports
twice and verifies that the saved checkpoint suppresses the second copy.
Destinations must be UTF-8 because the object-store interface takes text;
rejecting an invalid path is safer than silently exporting to a different one.

### One exporter at a time

Keying the checkpoint on content wasn't the whole story: the *archive* still used the plain filename, so a reused `logs_000000.parquet` overwrote its predecessor in S3. The object key now carries the SHA-256 too (`{node}/{sha256}-logs_000000.parquet`), so an archived object never changes. The checkpoint also records a hash of the destination and node it covers, because a receipt from bucket A says nothing about bucket B. Change either and the old receipts are cleared; the worst case is a repeated upload to an immutable key.

The last hole was concurrency. The periodic export, the disk-pressure loop, the API handler and an offline `relish logs-export` could each load the checkpoint, export, and save a stale copy over a newer one. Now they all go through `export_logs`, which takes a non-blocking lock on a separate `_export_checkpoint.lock` file (separate, because the checkpoint itself is replaced by an atomic rename), reloads the latest checkpoint, uploads, and persists before returning. Its tail:

```rust
let committed = tokio::task::spawn_blocking(move || {
    let _lock = lock;
    current.save(&path).map_err(|error| {
        KetchupError::Io(std::io::Error::other(format!(
            "files exported but checkpoint could not be saved: {error}; retry is safe"
        )))
    })?;
    Ok::<_, KetchupError>(current)
})
.await
.map_err(|error| KetchupError::Io(std::io::Error::other(error.to_string())))??;
*checkpoint = committed;
```

`move || { ... }` is a closure (an anonymous function) that takes ownership of what it captures: `lock`, `current` and `path`. If the async caller is cancelled, the blocking thread keeps running and still *owns* the lock, so no second exporter gets in before the rename finishes. `let _lock = lock;` keeps the lock alive until the closure ends; a name starting with `_` just silences the unused-variable warning. Writing `let _ = lock;` would be a bug, because a bare `_` binds nothing and the lock would drop (and unlock) on the spot.

`Ok::<_, KetchupError>(current)` uses the "turbofish" `::<>` to name the closure's error type, which the compiler can't infer by itself; `_` lets it fill in the rest. The `??` unwraps two layers: `spawn_blocking` returns an error if the thread panicked, and inside that sits our own `Result`. Finally, `*checkpoint = committed` writes through the caller's `&mut` reference, so the caller only ever sees a committed snapshot.

#### Busy is not broken

The V02 soak turned that lock into a wall of red. Every node's journal said `log export failed during disk pressure: io error: export checkpoint is busy` a few hundred times, with `log export error: ... busy` close behind. Was the soak losing logs?

No. `check_and_relieve` returns early whenever its export fails, and it only prunes a file whose exact bytes are in the checkpoint, so a busy lock left every file on disk. The cause was mundane: Bun starts the 60-second export timer and the 300-second disk-pressure timer together, and 300 is a multiple of 60, so every fifth export tick lands on a disk-pressure tick. One of the two loses the `try_lock`. When the periodic export lost, nothing happened: the other exporter was shipping the same files. When disk pressure lost, it skipped pruning for five minutes, which with an 8 MB cap is exactly when you want it to prune.

So a busy lock is now its own error variant, `KetchupError::ExportBusy`, instead of a stringly-typed `io::Error`. `std::fs::File::try_lock` already tells the two cases apart:

```rust
match lock.try_lock() {
    Ok(()) => {}
    Err(std::fs::TryLockError::WouldBlock) => return Err(KetchupError::ExportBusy),
    Err(std::fs::TryLockError::Error(error)) => return Err(KetchupError::Io(error)),
}
```

The periodic task matches `Err(KetchupError::ExportBusy) => {}` and stays quiet. Disk pressure waits for its turn instead:

```rust
loop {
    match export_logs(source_dir, destination, node_id, checkpoint).await {
        Err(KetchupError::ExportBusy) if tokio::time::Instant::now() < deadline => {
            tokio::time::sleep(EXPORT_BUSY_POLL).await;
        }
        other => return other,
    }
}
```

The `if` after the pattern is a *match guard*: the arm only matches when the pattern fits and the condition holds, so a busy error past the 60-second deadline falls through to `other` and is reported like any failure. Why poll every 100 ms rather than call the blocking `lock()` in `spawn_blocking`? Because a thread parked in `flock` can't be cancelled. If the caller gave up, the thread would still wake up later holding the lock with nobody to release it until it finished. A `tokio::time::sleep` is dropped cleanly on shutdown. The test holds the lock from outside, releases it after 300 ms, and checks that the same `check_and_relieve` call then exports and prunes the file.

`ExportBusy` also turned up where nobody was exporting. `checkpoint_retains_only_live_source_generations_across_restart` exports twice in a row, and in a full `cargo test --test suite` run the second export was refused as busy in four runs out of six (#519). It was the same `flock` quirk that Chapter 9 describes for quickstart operations. The lock belongs to the open file description, and a child process that another thread is halfway through spawning holds a copy of it until its `exec`. Closing our descriptor didn't release the lock, so the export straight after found it held. The lock became an `ExportLock` whose `Drop` calls `unlock()` before the file closes, which releases it for every copy at once. Today it's the shared `file_lock::FileLock` that Chapter 9 describes, which every file lock in the codebase uses. In production that was a spurious skip or a 100 ms wait, since Bun spawns workloads while it exports. In a test it was a failure.

### Whose disk is `./archive`?

The manual had two lines that read like one thought:

```sh
relish logs-export --dest ./archive
relish logs-search ./archive "SELECT count(*) FROM logs WHERE line LIKE '%error%'"
```

Run them on a laptop, against a node somewhere else, and see what happens. The first line prints `agent exported 1 file(s) (2399 bytes) to ./archive/local`, and the files are real. They're just on the node, in `./archive` relative to wherever Bun happened to start. The second line runs on your laptop, where there's no such directory. Our own test caught it in the act: the first failing run left an `archive/` folder in the repository root, because that was the working directory of the agent under test.

Remember the endpoint from earlier in this chapter, `POST /v1/logs/export`? We'd moved the work to the agent so that it could use the store's real path and its credentials. That was right for `s3://`. For a bare path it quietly changed whose disk the path meant, and every word the CLI printed still looked local.

There were two ways out. We could keep the files on the node and make the output say so, or make `--dest` mean what it looks like. A flag that takes `./archive` from a shell on your machine has only one sensible reading, and the second option turned out to cost two small read-only routes, so that's what we did. `GET /v1/logs/archive` lists the node's flushed Parquet files with each one's size and SHA-256, and `GET /v1/logs/archive/{name}` streams one. Relish writes them under `./archive/<node>/`. `s3://` and `gs://` still go to the agent, because it's the one holding the credentials.

Three decisions in there are worth a look.

**A copy isn't an export.** The checkpoint is what licenses pruning, so `relish logs-export` must not touch it: a file on someone's laptop is no reason for a node to delete its only copy. So how does a repeat run skip what it already has, with no checkpoint? We gave the files the names an export gives them, `{sha256}-{filename}`. A name with its hash in it answers the question by itself: if the file exists, those are the bytes. That only holds if a half-written file can never take the name, so Relish writes to a hidden `.partial` file, checks the size and hash, syncs, and only then renames.

**Neither side trusts the other's paths.** The API refuses any name that isn't a single `.parquet` path component, so `_export_checkpoint.json` and `../escape.parquet` get a 400. Relish does the same in the other direction, because the node's name and the file names in the listing are about to become local paths:

```rust
let mut components = Path::new(&listing.node_id).components();
let one_name = matches!(components.next(), Some(Component::Normal(_)));
if !one_name || components.next().is_some() {
    return Err(archive_error(format!(
        "the agent gave an unusable node name: {:?}",
        listing.node_id
    )));
}
```

`Path::components` splits a path the way the operating system would, into an enum with variants like `Normal`, `ParentDir` and `RootDir`, so `..` and `/tmp/x` can't be mistaken for names. `matches!` is a macro that turns a pattern into a `bool`. In Go you'd compare strings against `".."` and hope you'd thought of every spelling; here the type did the thinking.

**The archive's routes refuse a scoped token, even an Admin one.** `/v1/logs/query/{app}/{namespace}` can filter by tenant. A Parquet file can't: it's every tenant's lines in one blob. So these routes call `require_unscoped`, as `/v1/logs/sql` does. We also audit the listing as `logs.exported`, the same action the `POST` records, so moving the work to a `GET` didn't take it off the trail. The audit layer mostly records writes, so this is the one read it records; the files that follow each listing would only add noise. An Admin who already knows a file's name can fetch it without a listing, so this is a record of the normal path, not a wall.

Getting the files to the right machine wasn't the end of it. The end-to-end test copied them, ran the manual's second line, and got back `{"errors":0}`. No error at all. DataFusion's `listing_table_ignore_subdirectory` defaults to `true`, and an export files everything one level down in `<node>/`, so the search looked in `./archive`, found no Parquet directly in it and reported, accurately, zero rows. Chapter 11's example got away with this by pointing at `/tmp/backup/node-1/`; the manual never did. Both query paths now set it to `false`, so a search of an archive's root sees every node in it. A test that stopped at "the files arrived" would have shipped a manual that printed the wrong answer, which is worse than the error we started with.

### Errors that used to vanish

Retention in the metrics and rollup stores ignored `remove_file` errors and counted the file as deleted anyway; now only successful removals count. The exporter forgives exactly one read failure, `NotFound`, because retention can delete a file between listing the directory and opening it.

Log fan-out had two ownership bugs. Timing `request.send()` isn't enough, because that future finishes when the headers arrive and a peer can then stall mid-body, so the timeout now covers the body read too. And dropping a Tokio `JoinHandle` *detaches* its task rather than cancelling it, so an abandoned query left its requests running. `fan_out_query` now spawns into a `tokio::task::JoinSet`, which owns its tasks and aborts them all when it's dropped.

The alert inventory used to decode `{}` as "no alerts". Bun and Relish now share one response type with a required list:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsResponse {
    /// Observed statuses; a missing field is not evidence of an empty list.
    pub alerts: Vec<AlertStatus>,
}
```

Serde refuses to decode a struct when a field without `#[serde(default)]` is missing, so only an explicit `{"alerts": []}` now means "nothing is firing".

## What we learned

### Reuse the query engine, don't build one

DataFusion gives us SQL parsing, query planning, columnar execution, predicate pushdown, and Parquet I/O. That's roughly 200,000 lines of code we didn't write. Our glue layer is about 400 lines. The ratio (500:1) is the best leverage in the entire project.

The temptation was to build something simpler: a custom iterator over Parquet files with hardcoded filters. It would have been "enough" for v1. But then you want time-range queries, then aggregations, then LIKE filters, then JSON field extraction, and suddenly you've built half a query engine badly. Start with DataFusion and you skip the reinvention.

### Four default alerts cover 90% of incidents

We thought operators would want to define custom alert rules from day one. In practice, the four defaults (CPU throttle, OOM risk, memory high, disk high) catch nearly every production incident that metrics can detect. Custom rules still aren't supported, and nobody has complained yet.

The lesson: don't build config for things that have obvious defaults. Ship the defaults, add config later if someone needs it.

### "How far back do we look?" is not "how stale may this be?"

The first evaluator queried the last 120 seconds and kept the newest row per metric *name*, discarding the timestamp and the labels. Dropping the timestamp made the query window double as a freshness guarantee, so a metric that stopped arriving 110 seconds ago still looked live. Those are different questions, and they now have different names: `QUERY_WINDOW_SECS` and `MAX_VALUE_AGE_SECS`.

A freshness bound needs a "now" to measure from, and the first version read the wall clock inside `gather_latest_values`. That hid a flaky test ([#763](https://github.com/reliaburger/reliaburger/issues/763)). `tests/mayo_memory.rs` writes four hours of samples stamped against one `now` it reads at the start, then runs an alert read after each hour. The newest sample is ten seconds before the start of the current minute, so it starts out between 10 and 69 seconds old. Writing the history takes about 3 seconds on an idle Mac and 25 on a loaded one. Started late in a minute on a loaded host, the last read came more than 90 seconds after the newest sample and found "no fresh series". In a stress loop on a host with a load average near 60, 4 of 128 runs failed this way. The fix wasn't a longer bound. The function now takes the instant as an argument, `gather_latest_values(store, now)`, as the rollup generator's `generate(store, now)` already did. The alert loop in `bun` reads the clock once per tick and passes it in; the test passes the instant it wrote against. A function that reads the clock decides for its caller what time it is. Taking `now` as a plain `u64` keeps the decision with the caller, and lets a test name any instant it likes without a mock clock.

The `store` it takes changed too, from the store itself to the lock around it. On three Wyse 3040 thin clients, `relish nodes` sometimes said a healthy node "didn't answer" in its 3 seconds ([#2215](https://github.com/reliaburger/reliaburger/issues/2215)). Every 30 seconds the alert loop took the store's read lock and held it through the query, which reads a footer from every Parquet file in the directory: about 2,600 files, a few seconds on an Atom. A read lock alone wouldn't have mattered, since readers share it. But the collector asks for the write lock every 10 seconds, and tokio's `RwLock` is fair: once a writer is waiting, new readers queue behind it. So the node summary's read waited for the writer, which waited for the alert query. The fix splits the query in two. `MayoStore::snapshot_since` lists the files and copies the unflushed samples under the lock, and `MetricsSnapshot::query_sql` runs the SQL afterwards, holding nothing:

```rust
// One statement, so the read guard is dropped before the query runs.
let snapshot = store.read().await.snapshot_since(window_start).await;
```

The guard that `store.read().await` returns is a temporary, and Rust drops temporaries at the end of the statement that made them. So the lock is released at that semicolon, before the query starts. Taking the lock rather than a `&MayoStore` puts that rule in the function, where a caller can't get it wrong. The snapshot is safe to read without the lock because a published Parquet file never changes; the one thing that can happen meanwhile, retention deleting an old file, could already happen during a locked query, because pruning takes only the read lock too.

That fixed one caller. The audit found nine more doing the same thing: every metrics and log API handler, and the rollup worker, which held the guard through five window queries when backfilling ([#2449](https://github.com/reliaburger/reliaburger/issues/2449)). The log store's writer is the drain, which asks for the write lock once per line, so a slow `relish logs` stalled ingestion too. So the snapshot became the only way to query. `MayoStore::snapshot()` captures the file listing and a copy of the unflushed buffer and returns a `MetricsSnapshot`, and every read (`query`, `query_app`, `query_all`, `metric_names_since`, `query_window_aggregates` and the rest) is a method on the snapshot. `RollupStore::snapshot()` and `LogStore::snapshot()` do the same for rollups and logs. Each handler now starts with the same shape:

```rust
// One statement: the read guard is dropped before any query runs.
let store = match mayo.read().await.snapshot().await {
    Ok(snapshot) => snapshot,
    Err(error) => return metrics_store_error(error),
};
```

Watch out for the version that looks equivalent: `mayo.read().await.query(...).await` in a `match` scrutinee. Temporaries in a `match` scrutinee live until the end of the whole `match`, so that form holds the guard through the query *and* the arms. Splitting the snapshot into its own `let` is what makes the drop happen where you can see it. The store keeps its old query methods as one-line wrappers (`self.snapshot().await?.query(...)`) for callers that own the store outright, like tests and the startup code.

How do you test "doesn't hold a lock"? You make the query wait, and try to write. A test-only `QueryGate` on the store parks every query until the test releases it, after telling the test it has started. `a_metrics_write_is_not_blocked_while_a_rollup_is_generated` starts the rollup worker, waits for its query to reach the gate, and then asks for the write lock with a 200 ms timeout; `the_metrics_query_handler_releases_the_store_before_it_queries` does the same through the HTTP router. Before the change both timed out, because the guard was still held while the query sat at the gate.

Two callers outside the stores' own files still had the `match` form ([#2530](https://github.com/reliaburger/reliaburger/issues/2530)). `relish top` read its CPU and memory columns with `mayo.read().await.query_names_since(...).await` in a `match` scrutinee, exactly the trap above; it now takes a snapshot in its own `let`, in `latest_local_usage`. The autoscaler was worse. Once a tick, on the leader, it held the rollup store's read guard through one aggregate query per autoscaled app, and the aggregator's ingest and flush need the write lock. It now takes one snapshot for the whole tick and runs one query per metric series the apps use, at most two (CPU and memory). Apps can each ask for a different `evaluation_window`, so the query covers the longest one, and because the answer comes back per minute, each app then averages only the minutes inside its own window. Fifty autoscaled apps used to be fifty queries under the lock; now they're one or two with no lock. The rollup store got the same test-only `QueryGate`, and `the_autoscaler_queries_one_snapshot_per_series_holding_no_lock` checks both halves: a write goes through while the query is parked, and releasing the gate once is enough for the whole tick, where one query per app would park again and time out. `relish_top_releases_the_metrics_store_before_it_queries` does the write check for `relish top`.

Dropping the labels was worse. Node A reports 95% CPU; a second later node B reports 10%. Keep one reading per name and B's healthy number hides A's problem. So an alert is keyed on the rule *and* the series:

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AlertInstance {
    rule_name: String,
    labels: BTreeMap<String, String>,
}
```

The evaluator keeps a `BTreeMap<AlertInstance, AlertState>`. A `BTreeMap` is sorted, so its keys must be orderable, and deriving `PartialOrd` and `Ord` gives us that: Rust compares fields in declaration order, with no hand-written comparison. Each series now has its own pending timer, firing state and recovery, and only a fresh healthy reading from the *same* series resolves an alert. The labels reach the webhooks too, so resolving A can't close B's PagerDuty incident.

### An app that paged the operator

Per-series alerts opened a hole we found in the November 2026 audit (#2387). The rules choose series by metric name, and an app's scraped samples kept whatever names the app chose. So an app with `metrics = {}` whose `/metrics` page said `node_cpu_usage_percent{shard="0"} 99`, and so on up to the 20,000-sample scrape limit, stored 20,000 series named like the node's CPU gauge. Five minutes later `cpu_throttle` fired 20,000 times, Bun spawned a task per transition to post to every destination, and when the app was deleted the alerts stayed firing, because "firing with no reading" kept firing for ever.

Four changes close it. The scraper now treats a name the node writes itself the way it already treated a clashing label: `node_cpu_usage_percent` from an app is stored as `exported_node_cpu_usage_percent`. `is_reserved_metric_name` lists the node's prefixes (`node_`, `ingress_`, `ketchup_`, `bun_`, `mayo_`, `discovery_withdrawal_`) and, for prefixes apps use too, exact names: `process_cpu_percent` is the node's, but a client library's `process_cpu_seconds_total` keeps its name. `every_metric_the_node_writes_is_reserved` runs the collector and checks each name it produces, so a new node metric can't slip past the list. Second, as defence in depth, a rule on a `node_*` metric reads only series without an `app` label. Third, `Firing` now counts the evaluations in a row with no reading, and resolves at `MAX_MISSED_EVALUATIONS` (10):

```rust
(AlertState::Firing { since, missed }, None) => {
    if missed + 1 >= MAX_MISSED_EVALUATIONS {
        AlertState::Inactive
    } else {
        AlertState::Firing { since: *since, missed: missed + 1 }
    }
}
```

The pattern `Firing { since, missed }` binds both fields by reference, because we're matching on `&AlertState`; `*since` copies the `SystemTime` out, and `missed + 1` works on a `&u32` because the arithmetic operators are implemented for references to numbers too. Arms that don't care about the count write `Firing { since, .. }`, where `..` means "and any other fields", so adding the field touched only the arms that build a `Firing`. One reading resets the count, so a gap of a scrape or two still can't resolve an alert. Fourth, Bun hands each evaluation's transitions to `WebhookDispatcher::dispatch_all`, which runs them through `for_each_concurrent`, and every delivery takes a permit from a semaphore the dispatcher's clones share, so at most `MAX_CONCURRENT_DISPATCHES` (8) posts run at once however many evaluations overlap.

The same issue found a smaller leak in the per-app query. It picked an app's series with `labels LIKE '%"default/web"%'`, which matches that text anywhere in the labels JSON. So an app that labelled its own series `x="default/web"` showed up in `web`'s `relish metrics` answer and its dashboard charts. The labels are stored as `serde_json` of a `BTreeMap`, which writes no spaces, so the condition can name the key too: `labels LIKE '%"app":"default/web"%'`. A quote inside a label value is stored escaped as `\"`, so no value can contain that text, and `app_query_matches_only_the_app_label` stores a forged series beside the real one and expects only the real one back.

### What an alert remembers across a restart

The evaluator's states lived in memory, so every Bun restart started every alert at Inactive (#2478). A disk at 85% fired `disk_high` and paged someone; they freed space and upgraded the cluster, which restarts Bun; afterwards the disk read 60%, Inactive plus an in-range reading is no transition, and the incident stayed open on the pager while `GET /v1/alerts` showed nothing firing. Every self-upgrade restarts Bun, so this wasn't rare.

Now Bun keeps the firing alerts in `<data dir>/alerts-firing.json`: rule name, labels and the Unix second it started firing. It saves the file after any evaluation that produced a transition and restores it at start-up, before the first evaluation, skipping rules that no longer exist. Only firing state is worth keeping. A pending alert that restarts its timer fires a little later, which costs nothing; a firing one that's forgotten never resolves. A restored alert that never gets another reading still resolves after `MAX_MISSED_EVALUATIONS`. We considered sending a blanket "resolved" for every rule at start-up instead, and rejected it: it closes incidents whose condition still holds, and they don't come back until the rule's `for` window passes again.

The record is its own small type, separate from `AlertState`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FiringRecord {
    rule_name: String,
    labels: BTreeMap<String, String>,
    since_unix_secs: u64,
}
```

`AlertState` holds a `SystemTime`, which serde can serialise but in a shape that's awkward to read; storing whole seconds since the Unix epoch keeps the file obvious to a human with `cat`. The file is written atomically, through a temporary file and a rename, and the write runs under `spawn_blocking` because file I/O blocks. Bun awaits it before the next evaluation, so two saves can't land out of order. A file that's missing or doesn't parse restores nothing, with a line on stderr: before 1.0 there's no migration, and a bad file mustn't stop Bun starting. It's node-local state, not Raft state or a wire format, so the compatibility version doesn't change. `a_resolve_reaches_the_webhook_after_a_restart` fires an alert against a local webhook receiver, saves, builds a fresh evaluator from the file, feeds it an in-range reading and checks the receiver got `firing` then `resolved`.

### A test that passed for the wrong reason

A dependency advisory flagged the Thrift crate that Parquet used to decode file metadata: a crafted length could make it reserve absurd amounts of memory. Parquet 59 replaced Thrift with its own decoder, and DataFusion 55 uses Parquet 59, so we upgraded DataFusion by ten major versions. It changed two lines of ours, because we never name `parquet` in `Cargo.toml`; we use DataFusion's re-export (`datafusion::parquet::...`) and always get the version DataFusion was built against. We kept regression tests that corrupt a real Parquet file's metadata and check that the reader refuses it.

We ran them on a Mac, they passed, and we moved on. Then Linux CI aborted the entire test process: "memory allocation of 206158430112 bytes failed". The impossible-count test declares two billion schema elements, and Parquet 59 calls `Vec::with_capacity` with that count before checking there are bytes to back it. macOS happily hands out 206 GB of address space you never touch, and the decoder fails on the next byte with an ordinary error. Linux refuses the reservation, and Rust's default response to a failed allocation is to abort the process. The same corrupted archive that returned `Err` on a laptop would take down a production node.

Upstream had fixed it in Parquet 60, but DataFusion 55 still needs 59. So we forked arrow-rs, applied that one commit to the 59.3.0 tag, and pointed Cargo at the fork:

```toml
[patch.crates-io]
parquet = { git = "https://github.com/reliaburger/arrow-rs", rev = "34ac1864f214da1648b05fbd1a2b6de4f2b4a952" }
```

`[patch.crates-io]` replaces a crates.io package everywhere in the build, so DataFusion picks up the fixed decoder too. We patch all fifteen arrow-rs crates to the same revision: patching only `parquet` would build a second copy of the Arrow crates, and Rust treats a type from one copy as unrelated to the "same" type from the other.

A fork is a debt: every arrow-rs fix after 59.3.0 passes you by until you redo the patch. So we also asked upstream to backport the fix to the 59 line (apache/arrow-rs#11186), and they released Parquet 59.3.1 with it. In 0.2.1 we deleted the whole `[patch.crates-io]` block, and `Cargo.lock` now takes every arrow-rs crate from crates.io at 59.3.1 (#975). The regression tests didn't change, and they still run on Linux in CI, where the abort happened. The lesson: a test only proves something on the platform where it ran.

Leaving the fork paid off in 0.2.6. On 2 November, RUSTSEC-2026-0442 reported that arrow-data 59.3.1 can read past the end of a RunEndEncoded array's values. The 0.2.6 candidate's release audit refused it. Upstream had already released 59.3.2 on the same line, so the fix was `cargo update` on the fifteen arrow and parquet crates: a `Cargo.lock` change, with no code and no fork. Our own files never build RunEndEncoded arrays, but the advisory is in a crate that reads files from disk, so we took the patch rather than adding an exception to `.cargo/audit.toml`.

zstd hasn't been so easy. On 26 October RustSec published thirteen advisories against zstd 0.13.3 and zstd-safe 7.3.0, and zstd 0.14.3 fixes all of them. But Parquet's zstd codec on the 59 line still asks for `zstd = "^0.13"`, and in Cargo a caret requirement below 1.0 treats the minor version as the breaking one, so `^0.13` will never accept 0.14. Only Arrow 60 moves, and DataFusion 55 needs Arrow 59. 0.2.3 shipped with a dated exception for the thirteen IDs (#1366). Then we shrank it: our own source bundle (#1570) and rust-embed's `include-flate` moved to zstd 0.14, and a test in `tests/suite/dependency_audit.rs` now fails if anything except Arrow, Parquet and DataFusion depends on the old line. The two versions share one copy of the C library, `zstd-sys` 2.1.2 with libzstd 1.5.7, so the bytes we write didn't change. The Rust wrapper got a new major version, but the frames on disk are the same, so no log, metric or bundle needs migrating. The rest of the exception waits for DataFusion 56, which moves to Arrow 60. It's on a release branch but not yet on crates.io, and we won't ship a database engine from a git branch to save one audit entry.

### Server-rendered HTML with meta refresh beats React

The Brioche dashboard is a single server-rendered HTML page. No JavaScript framework, no API calls, no state management. The browser refreshes every 5 seconds. Total payload: 10KB. Time to first meaningful paint: zero seconds (it's all in the HTML response).

Could we build a nicer dashboard with React and WebSocket updates? Sure. But that's a separate build pipeline, a node_modules tree, a bundler, and an entire frontend ecosystem to maintain. The server-rendered approach gives us something that works today and costs nothing to maintain.

## Tests

Almost everything in this chapter is a pure data transform: a sample becomes a `RecordBatch`, a SQL string becomes rows, a threshold-and-duration becomes an alert state. Pure transforms are the easy case for testing — no I/O, no async, no cluster. So Phase 6 leans almost entirely on unit tests, and there's a lot of them.

### Unit tests — the bulk of the work

The three subsystems carry their own tests at the bottom of each source file:

- **Mayo (metrics):** Arrow schema validation, DataFusion SQL over the metrics table, Parquet round-trips, Prometheus text parsing, and the alert state machine. The alert tests read like the transition table itself — `inactive_to_pending_on_breach`, `pending_to_firing_after_duration`, `firing_to_inactive_on_recovery`, `pending_to_inactive_on_recovery`, `missing_metric_does_not_fire`. Each builds an evaluator, feeds it a metric value, and asserts the resulting state. The hardening work added a matching set of failure-path tests, one per edge from the previous section: `query_metric_name_injection_is_neutralised` and `app_metrics_name_injection_cannot_bypass_predicate` (the SQL escape), `resent_window_does_not_double_count` and `restart_resumes_flush_counter_without_clobbering` (idempotent, durable rollups), `stale_telemetry_does_not_resolve_a_firing_alert` (the value-not-boolean state machine), `slack_payload_matches_provider_shape` and `pagerduty_payload_matches_events_v2_shape` (the provider webhook contracts), and `query_proceeds_during_flush` plus `corrupt_parquet_file_does_not_fail_query` (the off-lock flush and corrupt-file skip). Each names the failure it prevents. The streaming work added `every_read_matches_the_eager_golden_answers` and `every_rollup_read_matches_the_eager_golden_answers` (the answers recorded before the change), `streamed_reads_match_the_eager_reader` (the property test), and `a_disjunction_narrows_to_the_hull_of_its_sides` with its neighbours in `scan.rs`, which plan real SQL and check the bounds the scan receives. The memory tests live in their own binary, `tests/mayo_memory.rs`, because they replace the global allocator, and they need a process each (nextest's default) because the counters are process-wide.
- **Ketchup (logs):** `append_and_query`, grep/tail/time-range filters, and the SQL path (`app` filter, time range, `LIKE` grep, `LIMIT`). The log-path hardening added a matching set of failure tests, one per edge above: `bounded_sql_rejects_non_select`, `bounded_sql_rejects_other_tables` and `bounded_sql_caps_returned_rows` (the seatbelt on `/v1/logs/sql`); `unreachable_node_is_a_partial_failure` and `grep_value_with_ampersand_and_question_mark_transmitted_intact` (honest, correctly-encoded fan-out); `identical_lines_from_two_replicas_both_survive` and `repeated_identical_lines_from_one_node_both_survive` (the `(node, sequence)` dedup identity); `tail_returns_the_newest_lines_in_emission_order`, `lines_within_one_second_keep_emission_order_across_flushes` and `lines_sharing_a_second_come_back_in_sequence_order` (the V02 ordering bugs); `a_failed_check_does_not_end_the_follow` and `a_failed_state_check_keeps_a_running_instance_in_the_log_store` (a follower that only stops on evidence); `restart_does_not_reingest_lines_already_flushed`, `lines_lost_with_the_buffer_are_ingested_again_after_a_crash` and `refollowing_a_capture_file_replays_the_same_positions_and_the_store_keeps_one_copy` (exactly-once ingestion across restarts); `logs_from_several_instances_name_each_line_s_instance` (labelled tails); `reused_filename_with_new_contents_is_not_skipped` (durable checkpoint ids); and `flush_shared_persists_the_buffer_on_shutdown` (the final flush on stop).
- **Brioche (dashboard):** HTML rendering, and two security-flavoured tests worth calling out — `render_app_detail_escapes_html` (no stored-XSS through an app name) and `render_app_detail_masks_encrypted_env` (a secret never reaches the page). These are unit tests because the renderer is a pure function from data to a string; you assert on the string.

### End-to-end: the demo script

Unit tests prove each transform. To watch the whole pipeline breathe — collect, store, query, render — there's a script:

```sh
make observability-demo
```

It builds and starts `bun`, waits about twenty seconds (two ten-second collection cycles) so real CPU and memory samples accumulate, then queries the metric names, the summary, and the alert list over the HTTP API, and finally prints the dashboard URL. It's the fastest way to confirm the chapter's code actually works on your machine, not just in the test harness.

### Running them

Everything here runs under a plain `cargo test`. No gated tests in this chapter — no root, no eBPF, no network, no platform-specific runtime. To run a single subsystem:

```sh
cargo test --lib mayo        # metrics
cargo test --lib ketchup     # logs
cargo test --lib brioche     # dashboard
make observability-demo      # live, end-to-end
```

The cross-node and aggregation pieces — querying logs across the whole cluster, hierarchical metric rollups, exporting to S3 — are *advanced* observability, and their integration tests (`tests/suite/metrics_aggregation.rs`, `tests/suite/logs_cross_node.rs`, `tests/suite/log_export.rs`) belong to Chapter 11. This chapter is the single-node foundation they build on.

All of these run in the portable suite: `make test` (which drives them through nextest). No root, no eBPF, no network, no platform-specific runtime, and no fixed sleeps — the flush concurrency test drives both the write and the read to completion with `tokio::join!` rather than guessing at a delay. Chapter 15 covers the suite taxonomy and why a test that can pass without executing its promised behaviour is worse than no test.
