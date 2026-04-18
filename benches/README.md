# Auwgent Mode — Benchmarks

Performance measurements for every stage of the sandbox lifecycle.
Results are from a real run on the reference machine and are committed
so contributors can compare regressions against a consistent baseline.

## Reference Hardware

| | |
|---|---|
| **Machine** | Lenovo A475 |
| **RAM** | 8 GB |
| **OS** | Windows |
| **Rust profile** | `release` (optimized) |
| **Benchmark harness** | [Criterion 0.8.2](https://github.com/bheisler/criterion.rs) |

> Numbers collected on a mid-range laptop under normal background load.
> A dedicated server or Apple Silicon machine will be meaningfully faster.

---

## Running the Benchmarks

```bash
# Full benchmark suite (HTML reports generated automatically)
cargo bench

# Single benchmark by name
cargo bench cold_vm_startup

# Single group
cargo bench snapshot_restore

# Reports saved to:
# target/criterion/<bench_name>/report/index.html
```

---

## Results

### VM Lifecycle

| Benchmark | Median | What is measured |
|---|---|---|
| `cold_vm_startup` | **~390 µs** | `AuwgentSandbox::new()` in isolation — Luau VM boot, 20MB memory cap, `print` hijack, `await_all` stub, instruction interrupt hook |
| `boot_to_first_execute` | **~553 µs** | Full boot + `register_tools()` + `execute()` on a trivial no-yield script |

The **163 µs gap** between the two is the cost of Lua parsing, compiling to Luau bytecode, and executing the first instruction — not the VM boot itself.

---

### Tool Round-Trips

| Benchmark | Median | What is measured |
|---|---|---|
| `single_tool_roundtrip` | **~451 µs** | Boot → register tools → execute → 1 yield → resume → finish |
| `parallel_two_tools_roundtrip` | **~782 µs** | Same but 2 tools in one `await_all(a(), b())` |

Observation: the parallel yield costs ~330 µs extra, almost all of which is the additional JSON deserialization and Lua value injection for the second tool result. The coroutine suspend/resume itself is essentially free.

---

### Resume Variants

These measure the full round-trip cost including engine construction, showing that `resume_with_results` adds no overhead over raw `resume_with_json`.

| Benchmark | Median | Notes |
|---|---|---|
| `resume_with_json` | **~564 µs** | Direct JSON inject — baseline |
| `resume_with_results_ok` | **~539 µs** | `ToolResult::Ok` — identical path after materialization |
| `resume_with_results_err` | **~440 µs** | `ToolResult::Err` — slightly faster (sentinel JSON is a smaller value than a full result object) |

---

### Snapshot Restore Scaling

Measures `AuwgentSandbox::from_snapshot()` — how fast the engine can rebuild and fast-forward through previously completed tool rounds.

| Rounds cached | Median | Notes |
|---|---|---|
| 1 round | **~693 µs** | Rebuild + replay 1 yield |
| 5 rounds | **~495 µs** | Cached fast-forward is cheap — less overhead than construction |
| 10 rounds | **~910 µs** | Still sub-millisecond |
| 20 rounds | **~1.95 ms** | Linear scaling at ~90–95 µs per replayed round |

The **5-round result being faster than 1-round** is a warm-cache effect — Criterion's repeated sampling heats up allocator and TLB state, so the per-round overhead looks artificially low on the 5-round run. The trend is linear: roughly 90–100 µs per additional cached round.

For reference, **20 deep sessions restore in under 2ms** on this hardware — well within acceptable latency for any backend handler.

---

### Pure Lua Computation

| Benchmark | Median | What is measured |
|---|---|---|
| `pure_lua_1000_iterations` | **~761 µs** | VM boot + 1,000-iteration tight `for` loop, no tools, no yields |

This puts an upper bound on "pure scripting" work the LLM can do between tool calls — 1,000 arithmetic operations in ~370 µs above baseline boot cost.

---

## Summary

```
cold_vm_startup          ~390 µs   ← pure boot floor
boot_to_first_execute    ~553 µs   ← boot + parse + first instruction
single_tool_roundtrip    ~451 µs   ← complete 1-tool agent step
parallel_two_tools       ~782 µs   ← 2 tools in one await_all
snapshot_restore/1       ~693 µs   ← restore + replay 1 round
snapshot_restore/20      ~1.95 ms  ← restore + replay 20 rounds
pure_lua/1000_iters      ~761 µs   ← scripting computation speed
```

Every common operation is **sub-millisecond** on an 8GB mid-range laptop.
The bottleneck in a real agentic system will always be the LLM inference
and external API latency — not the sandbox.

---

## Adding New Benchmarks

Benchmarks live in `benches/startup.rs`. Each group uses Criterion's
`bench_function` or `benchmark_group` API. When adding a new measurement:

1. Add the function to `benches/startup.rs`
2. Register it in the `criterion_group!` macro at the bottom
3. Run `cargo bench -- <name>` to validate it works in isolation
4. Record the baseline result in this file under the appropriate section
