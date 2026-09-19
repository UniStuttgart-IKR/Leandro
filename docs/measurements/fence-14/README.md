<!-- SPDX-License-Identifier: MIT -->
# Fence-wait fallback (question 14)

Historical display-guest measurements; see [question 14](../../OPEN-QUESTIONS.md).

- `fencetime` submits an empty Vulkan batch and waits for its fence, ten times.
- Observed waits: about 0.06 ms when notified; 10.07 ms on fallback.
- Across four traced runs, waits of at least 10 ms matched 10 ms `poll` timeouts:
  0, 0, 2 and 0 occurrences. This identifies the delay, not its cause.

```text
poll([{fd=23</dev/nvidia0>, events=POLLIN|POLLPRI}], 1, 10) = 0 (Timeout) <0.010172>
```

| File | Contents |
|---|---|
| `plain.txt` | Six untraced runs; the slow wait is usually first |
| `y1..y4-fence.strace.txt` | Fence calls from `strace -f -y -T` |
| `strace-y-runs.txt` | Probe output for the traced runs |

- With `strace -f`, blocked calls split into unfinished/resumed lines. The timeout
  duration appears on the resumed line.
- The recorded display rig contained Vulkan and `fencetime`; its compute image did not.
