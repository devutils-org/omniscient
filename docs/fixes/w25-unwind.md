# Stream unwind, the twenty-fifth wave

Branch `feat/w25-unwind`. Done sequentially by one agent, in the main
checkout.

## Outside the owned files, said first

- `src/request.rs` (the `KeyKind` seam): `object`, appended.
- `src/emit.rs` (emit's): `derive_object`, beside `derive_func`.
- `src/collect/cpu/profile.rs`, `src/collect/sched/offcpu.rs`: their
  provenance fills `objects`.
- `catalog/cpu.yaml`, `catalog/mem.yaml`: `object` beside `func`, `within`,
  `insn` in the keys of every sampling source with stacks.

## What landed

1. **`Unwinder::perf_map_file` and `executable` giving cpu its provenance**:
   already done by stream cpu (`cpu.profile`'s provenance reads them); the
   same accessors now give `sched.offcpu` its own (stream sched).
2. **Every frame's object** (`Frame::object`, `Stack::objects_root_first`,
   `Provenance::objects`): the file's name, `[kernel]`, or none; carried
   into a result beside `stacks` and pruned with them.
3. **`--keys object`** (perfrecord 4, emit's request): the leaf frame's
   object, `perf report --sort dso`.

## Shown vs reasoned

- `cpu.profile -k object -e topk:5` of a Python `json.dumps` loop (shown):
  `libpython3.14.so.1.0` 35, `ld-linux-x86-64.so.2` 8,
  `_json.cpython-314-x86_64-linux-gnu.so` 5, `libc.so.6` 1. A unit test in
  `emit.rs` checks two functions of one library are one row and a leaf in
  no mapping is `object:?`, counted.

## Not done

1. **The exec re-read in `docs/unwind.md`**: the documents are stream docs'.

## Tests

`--lib emit stack request collect::cpu collect::sched::offcpu`, `--test
aggregation --test encoders --test cli --test recorder --test catalog_model
--test unwind --test stack_capture --test family_cpu`, clippy, fmt.

## Decision lines for `docs/DECISIONS.md`

- A frame's object travels in the provenance, frame for frame with the
  labels, so a label stays the function's name and `--keys object` reads the
  file without parsing it.
