<!-- SPDX-License-Identifier: MIT -->
# Naming the fence-wait fallback (OPEN-QUESTIONS 14)

`fencetime` is an empty `vkQueueSubmit` followed by `vkWaitForFences`, ten
times. A woken guest answers in ~0.06 ms; a guest that falls back answers in
10.07 ms, and this entry had never identified what produces that number.

`strace -T` identifies it without instrumenting anything: it prints how long
every syscall took, and the fallback is a syscall that TOOK 10 ms.

  * `plain.txt` -- six untraced runs. The slow wait is almost always the
    FIRST of the ten.
  * `y1..y4-fence.strace.txt` -- `strace -f -y -T`, filtered to the fence path. `-y` names the FD, `-f` follows
    the driver's threads. NOTE that `-f` SPLITS a blocking call into
    `<unfinished ...>` and `<... poll resumed>`, so the timing-out polls are
    on the *resumed* lines; a grep for `poll(...) = 0` on whole lines finds
    nothing and that is a trap, not an absence.
  * `strace-y-runs.txt` -- what each traced run reported, to line up against
    the timeouts.

The finding:

    poll([{fd=23</dev/nvidia0>, events=POLLIN|POLLPRI}], 1, 10) = 0 (Timeout) <0.010172>

and across the four traced runs the count of `>= 10 ms` waits equals the
count of 10 ms poll timeouts exactly (0, 0, 2, 0).

Run under the guest the `display` gate leaves up with `--keep-vm`; the
probes live on the display rig only, which is why an earlier attempt on a
compute guest found no `fencetime` and no Vulkan at all.
