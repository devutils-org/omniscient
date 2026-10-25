# Suzanne: narrow VFS probes before attaching

The complete Suzanne session compares the final PNG's writes, but used to
attach both `vfs_read` and `vfs_write` entry/exit programs. Those programs fire
system-wide before their process filter can reject a call. Repeated comparisons
reported kernel recursion misses in `io.fs`; even when PNG bytes matched, the
capture correctly remained incomplete.

An `io.fs` filter requiring `op == "read"` or `op == "write"` now selects the
matching probe pair before attachment. Only equalities in the top-level
conjunction are used; OR and NOT keep both pairs. The normal event filter still
runs. Suzanne explicitly requests writes, matching the scope of its comparison.
This does not collect VFS reads; request-tagged block activity remains separate.
The entry program also rejects unrelated tasks before reading file/inode fields.
Kernel misses remain counted, and comparison still fails on any actual loss.
This reduces probe exposure; it cannot promise zero misses under arbitrary load.

An experiment wrapping BPF bodies in preemption kfuncs still showed a skipped
invocation and was removed rather than offered as a fix.

The same validation exposed an unrelated memory polling race. A process with
PF_EXITING may still have state R/S while its mm is already unavailable. That
state now ends polling rather than counting ESRCH as a failed read. ESRCH from
a genuinely live process still gets the existing bounded retry and can fail.

Validation covers branch placement before the source gate, safe filter
selection through AND/OR/NOT, exiting task state, known loop counts and bytes
with and without the write filter, and repeated real CUDA renders.
