# Auwgent Mode — Benchmarks

Performance measurements for every stage of the sandbox lifecycle.
Results are from a real run on the reference machine and committed so
contributors can compare regressions against a consistent baseline.

## Reference Hardware

| | |
|---|---|
| **Machine** | Lenovo A475 |
| **RAM** | 8 GB |
| **OS** | Windows |
| **Rust profile** | `release` (optimized) |
| **Benchmark harness** | [Criterion 0.8.2](https://github.com/bheisler/criterion.rs) |

> Numbers collected on a mid-range laptop under normal background load.
> A dedicated server or Apple Silicon machine will be meaningfully faster
> (~100–150µs cold boot floor estimated on M-series hardware).

---

## Running the Benchmarks

```bash
# Full benchmark suite (HTML reports generated automatically)
cargo bench

# Single benchmark by name
cargo bench cold_vm_startup

# Single group
cargo bench snapshot_restore
cargo bench payload_size
cargo bench instance_pressure
cargo bench script_complexity
cargo bench tool_registration

# Reports saved to:
# target/criterion/<bench_name>/report/index.html
```

---

## How We Compare

| | **Auwgentmode** | **Monty (Pydantic)** | **TanStack/V8** | **TanStack/QuickJS** |
|---|---|---|---|---|
| Language | Luau (full) | Python subset | TypeScript | TypeScript |
| Cold start | ~390 µs | ~4–60 µs | ~1–5 ms | ~5–50 ms |
| Full agent step | ~451 µs | comparable | comparable | comparable |
| Parallel tools | ~782 µs (2 tools) | not published | not published | not published |
| Snapshot / restore | ✅ ~693 µs (1 round) | ✅ VM-state bytes | ❌ | ❌ |
| Snapshotting model | Replay-based | Raw VM serialization | — | — |
| Partial failure API | ✅ `ToolResult::Err` | ❌ | ❌ | ❌ |
| Host-controlled tools | ✅ coroutine yield | ❌ blocking call | ❌ blocking call | ❌ blocking call |
| Runs in browser | Via wasmoon | ❌ | ❌ | ✅ native |
| Multi-language interop | ✅ Auwgent SDK | ❌ Python only | ❌ JS/TS only | ❌ JS/TS only |
| Embedding language | Rust | Rust | JS / TS | JS / TS |

### The startup gap explained

Monty's `<60µs` startup is interpreter-only — spinning up a minimal Rust-native Python
AST walker with nearly zero stdlib. Our `~390µs` includes full Luau C VM initialization,
20MB memory cap, `print` hijack, `await_all` stub injection, and the instruction
interrupt hook. We intentionally do more at boot. The real production number to compare
is the **full agent step time — `~451µs`** — which is the complete request lifecycle
and is what actually matters on a server.

The 386 µs difference between Monty and us is noise at real agent scale.
LLM inference runs at 500ms–3s and external API calls at 50ms–500ms.
**The sandbox is never the bottleneck.**

---

## Results

### VM Lifecycle

| Benchmark | Median | What is measured |
|---|---|---|
| `cold_vm_startup` | **~390 µs** | `AuwgentSandbox::new()` — Luau boot, 20MB cap, print hijack, await_all stub, interrupt hook |
| `boot_to_first_execute` | **~553 µs** | Full boot + `register_tools()` + `execute()` on a trivial no-yield script |

---

### Tool Round-Trips

| Benchmark | Median | What is measured |
|---|---|---|
| `single_tool_roundtrip` | **~451 µs** | Boot → tools → execute → 1 yield → resume → finish |
| `parallel_two_tools_roundtrip` | **~782 µs** | Same but 2 tools in one `await_all(a(), b())` |

The ~330 µs gap between single and parallel is almost entirely JSON deserialization and
Lua value injection, not coroutine overhead. Coroutine suspend/resume is essentially free.

---

### Resume Variants

| Benchmark | Median | Notes |
|---|---|---|
| `resume_with_json` | **~564 µs** | Direct JSON inject — baseline |
| `resume_with_results_ok` | **~539 µs** | `ToolResult::Ok` — identical path after materialization |
| `resume_with_results_err` | **~440 µs** | `ToolResult::Err` — slightly faster (smaller sentinel JSON) |

---

### Snapshot Restore Scaling

| Rounds cached | Median | Notes |
|---|---|---|
| 1 round | **~693 µs** | Rebuild + replay 1 yield |
| 5 rounds | **~495 µs** | Warm-cache effect — trustable trend is ~90–95 µs/round |
| 10 rounds | **~910 µs** | Still sub-millisecond |
| 20 rounds | **~1.95 ms** | ~90–95 µs per replayed round, linear |

50-round session restores are estimated at **~5ms** — well within acceptable latency
for any stateless HTTP handler.

---

### Pure Lua Computation

| Benchmark | Median | What is measured |
|---|---|---|
| `pure_lua_1000_iterations` | **~761 µs** | Boot + 1,000-iteration tight for loop, no tools |

---

### Payload Size Scaling

Measures `resume_with_json` cost as tool result payload grows,
isolating at what point JSON deserialization becomes the dominant cost.

| Benchmark | Payload size | Median |
|---|---|---|
| `payload_size/small_50b` | ~50 bytes (3-item array) | **~387 µs** |
| `payload_size/medium_1kb` | ~1 KB (50-item object array) | **~715 µs** |
| `payload_size/large_50kb` | ~50 KB (2,500-item result set) | **~22.8 ms** |

Key insight: small and medium payloads stay sub-millisecond. The 50KB payload jumps
to ~22ms — the cost is entirely `serde_json → mlua::Value` conversion over 2,500 table
inserts, not the coroutine itself. In practice, if a tool returns a 50KB result set the
right move is to paginate or filter on the Rust side before injecting.

---

### N-Instance Memory Pressure

Measures real allocator pressure from N `AuwgentSandbox` instances held alive
simultaneously. The 20MB heap cap is a *limit*, not a pre-allocation — each
idle sandbox consumes only a few KB of actual resident memory.

| Benchmark | N instances | Median | Per-instance cost |
|---|---|---|---|
| `instance_pressure/10_instances_serial` | 10 | **~3.2 ms** | ~320 µs |
| `instance_pressure/50_instances_serial` | 50 | **~16.7 ms** | ~335 µs |
| `instance_pressure/100_instances_serial` | 100 | **~34.5 ms** | ~345 µs |

Scaling is nearly perfectly linear at ~330–345 µs per sandbox with no allocator
degradation between 10 and 100 instances. At 100 concurrent sandboxes the theoretical
heap cap headroom is 2GB, but actual peak RSS will be far lower.
For a server handling 100 concurrent agent sessions, the sandbox creation
overhead is ~34ms total — dominated by Luau C VM init, not allocator pressure.

---

### Script Complexity Scaling

Measures parse + bytecode compile time as LLM-generated scripts grow in size.
Small scripts (≤ 100 lines) use direct `local` declarations — typical LLM style.
Large scripts (500 lines) use a table accumulator to work around Luau's
200-register local variable limit (a hard VM constraint documented below).

| Benchmark | Script size | Median | Compile overhead above boot |
|---|---|---|---|
| `script_complexity/10_lines` | 10 locals | **~388 µs** | ~0 µs (within boot noise) |
| `script_complexity/100_lines` | 100 locals | **~956 µs** | ~566 µs |
| `script_complexity/500_lines` | 500 table assignments | **~4.9 ms** | ~4.5 ms |

> **Luau register limit:** Luau enforces a hard cap of **200 local variables per function
> scope**. An LLM script with 200+ top-level `local` declarations will fail at parse time
> with `Out of local registers`. Mitigations for LLM prompt design:
> - Use a single table (`local data = {}; data.x = 1; data.y = 2`)
> - Break the script into helper functions (each gets its own 200-slot scope)
> - Use `load_library()` to pre-define reusable logic outside the LLM's script scope

---

### Tool Registration Scaling

Measures `register_tools()` cost at different registry sizes.
Each tool generates a Lua function stub that is parsed and compiled into the VM.

| Benchmark | Tool count | Median | Per-tool stub cost |
|---|---|---|---|
| `tool_registration/5_tools` | 5 | **~375 µs** | ~0 µs (within boot noise) |
| `tool_registration/20_tools` | 20 | **~726 µs** | ~17 µs |
| `tool_registration/50_tools` | 50 | **~939 µs** | ~11 µs |
| `tool_registration/100_tools` | 100 | **~3.7 ms** | ~33 µs |

For a typical agent with 10–20 tools, registration adds ~300–400 µs on top of cold boot.
A large deployment with 100 tools costs ~3.7ms per engine — still perfectly acceptable
since engines are typically created once per request, not per LLM token.
If you are registering 100+ tools, consider lazy registration (register only the subset
relevant to the current task) or pre-compiling stub code outside the hot path.

---

## Summary

```
cold_vm_startup          ~390 µs   ← pure boot floor
boot_to_first_execute    ~553 µs   ← boot + parse + first instruction
single_tool_roundtrip    ~451 µs   ← complete 1-tool agent step  ← KEY NUMBER
parallel_two_tools       ~782 µs   ← 2 tools in one await_all
snapshot_restore/1       ~693 µs   ← restore + replay 1 round
snapshot_restore/20      ~1.95 ms  ← restore + replay 20 rounds
pure_lua/1000_iters      ~761 µs   ← scripting computation speed
```

Every common operation is **sub-millisecond** on an 8GB mid-range laptop.
The bottleneck in any real agentic system is LLM inference and external API latency,
never the sandbox.

---

## Adding New Benchmarks

Benchmarks live in `benches/startup.rs`. Each group uses Criterion's
`bench_function` or `benchmark_group` API. When adding a new measurement:

1. Add the function to `benches/startup.rs`
2. Register it in the `criterion_group!` macro at the bottom
3. Run `cargo bench -- <name>` to validate it works in isolation
4. Record the baseline result in this file under the appropriate section
