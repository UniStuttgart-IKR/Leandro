# SPDX-License-Identifier: MIT
# sample.sh, from the run -- sottr-4q policy, 2026-08-21.
#!/usr/bin/env bash
# 1 Hz host sampler for the two-4Q SOTTR run.
O=/tmp/claude-1000/-home-silas-git-Leandro/f3fc191a-453c-4f97-8ba3-de299d29d9e3/scratchpad/sottr-bench; P15=554326; P17=556683
echo "ts,used_mib,free_mib,util_gpu,util_mem,sm_clk,mem_clk,temp_c,power_w,enc_util,dec_util,ram_avail_mib,vm15_mib,vm17_mib" > $O/host-1hz.csv
while :; do
  g=$(nvidia-smi --query-gpu=memory.used,memory.free,utilization.gpu,utilization.memory,clocks.sm,clocks.mem,temperature.gpu,power.draw,utilization.encoder,utilization.decoder --format=csv,noheader,nounits | head -1 | tr -d ' ')
  a=$(nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits | tr -d ' ')
  r=$(free -m | awk '/^Mem:/{print $7}')
  v15=$(awk -F, -v p="$P15" '$1==p{print $2}' <<<"$a"); v17=$(awk -F, -v p="$P17" '$1==p{print $2}' <<<"$a")
  echo "$(date +%H:%M:%S),$g,$r,${v15:-0},${v17:-0}" >> $O/host-1hz.csv
  sleep 1
done
