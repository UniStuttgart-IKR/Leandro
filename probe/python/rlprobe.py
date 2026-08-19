#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# rlprobe.py -- an RL workload probe: the boundary-crossing-heavy case.
#
# Classic REINFORCE: env on the CPU (numpy CartPole), policy on the GPU.
# Two boundary crossings per step (obs H2D, action D2H), two more per
# update (returns H2D, loss D2H). No gymnasium needed.
#
# Stages (cumulative, like torchprobe -- the delta between two runs
# isolates the increment):
#   0  cuInit
#   1  + policy onto the GPU
#   2  + one episode of rollout (inference only, many H2D/D2H)
#   3  + one training step (backward/optimizer/returns transfer)
#   4  + NVRL_EPISODES episodes of training (the steady-state gate)
#
# Switches:
#   NVRL_STAGE=0..4     (default 4)
#   NVRL_EPISODES=N     (default 50)
#   NVRL_PIN=1          obs through a pinned buffer -> cudaHostRegister /
#                       OS_DESCRIPTOR 0x71 over escape 0x27
#   NVRL_NONBLOCK=1     non_blocking=True on the H2D copies
#   NVRL_SYNC=1         torch.cuda.synchronize() after every env step
#   NVRL_ITEM=0         fetch the action via .cpu() instead of .item()
#
# What it is expected to show: NO cudaMallocManaged (the caching allocator
# takes cudaMalloc), every transition an explicit copy; the semaphore pool
# stays the only CPU-visible UVM mapping. The run is meant to confirm or
# refute exactly that.
#
# Note on steady state: episode lengths vary -> the caching allocator can
# still grow early on (new block sizes); it saturates because length <= 500.

import os
import time

import numpy as np
import torch
import torch.nn as nn

STAGE    = int(os.environ.get("NVRL_STAGE", "4"))
EPISODES = int(os.environ.get("NVRL_EPISODES", "50"))
PIN      = os.environ.get("NVRL_PIN", "0") == "1"
NONBLOCK = os.environ.get("NVRL_NONBLOCK", "0") == "1"
SYNC     = os.environ.get("NVRL_SYNC", "0") == "1"
ITEM     = os.environ.get("NVRL_ITEM", "1") == "1"

dev = None
pinbuf = None
H2D = 0
D2H = 0


class CartPole:
    """Minimal CartPole, same dynamics as gym CartPole-v1, pure CPU/numpy."""

    def __init__(self, seed=0):
        self.rng = np.random.default_rng(seed)
        self.g, self.mc, self.mp = 9.8, 1.0, 0.1
        self.l, self.fmag, self.tau = 0.5, 10.0, 0.02
        self.x_lim = 2.4
        self.th_lim = 12 * np.pi / 180
        self.max_steps = 500

    def reset(self):
        self.s = self.rng.uniform(-0.05, 0.05, size=4).astype(np.float32)
        self.t = 0
        return self.s.copy()

    def step(self, a):
        x, xd, th, thd = self.s
        f = self.fmag if a == 1 else -self.fmag
        ct, st = np.cos(th), np.sin(th)
        mt = self.mc + self.mp
        tmp = (f + self.mp * self.l * thd * thd * st) / mt
        thacc = (self.g * st - ct * tmp) / (self.l * (4.0 / 3.0 - self.mp * ct * ct / mt))
        xacc = tmp - self.mp * self.l * thacc * ct / mt
        x, xd = x + self.tau * xd, xd + self.tau * xacc
        th, thd = th + self.tau * thd, thd + self.tau * thacc
        self.s = np.array([x, xd, th, thd], dtype=np.float32)
        self.t += 1
        done = abs(x) > self.x_lim or abs(th) > self.th_lim or self.t >= self.max_steps
        return self.s.copy(), 1.0, done


class Policy(nn.Module):
    def __init__(self, hidden=128):
        super().__init__()
        self.net = nn.Sequential(
            nn.Linear(4, hidden), nn.Tanh(), nn.Linear(hidden, 2)
        )

    def forward(self, x):
        return self.net(x)


def to_gpu(obs):
    """obs (numpy, CPU) -> GPU tensor. The H2D crossing per env step."""
    global H2D, pinbuf
    H2D += 1
    if PIN:
        if pinbuf is None:
            pinbuf = torch.empty(4, dtype=torch.float32, pin_memory=True)
        pinbuf.copy_(torch.from_numpy(obs))
        return pinbuf.to(dev, non_blocking=NONBLOCK)
    return torch.from_numpy(obs).to(dev, non_blocking=NONBLOCK)


def rollout(env, pol, train):
    global D2H
    obs = env.reset()
    logps, rewards = [], []
    done = False
    while not done:
        x = to_gpu(obs)                              # H2D
        logits = pol(x)                              # GPU-Forward
        dist = torch.distributions.Categorical(logits=logits)
        a_gpu = dist.sample()
        if train:
            logps.append(dist.log_prob(a_gpu))
        D2H += 1
        a = int(a_gpu.item()) if ITEM else int(a_gpu.cpu().numpy())  # D2H + Sync
        if SYNC:
            torch.cuda.synchronize()
        obs, r, done = env.step(a)                   # CPU physics
        rewards.append(r)
    return logps, rewards


def update(pol, opt, logps, rewards, gamma=0.99):
    global H2D, D2H
    # Compute the returns on the CPU deliberately, then move them over in one go
    G, rets = 0.0, []
    for r in reversed(rewards):
        G = r + gamma * G
        rets.append(G)
    rets.reverse()
    rets_t = torch.tensor(rets, dtype=torch.float32)
    rets_t = (rets_t - rets_t.mean()) / (rets_t.std() + 1e-8)
    H2D += 1
    rets_gpu = rets_t.to(dev, non_blocking=NONBLOCK)  # H2D
    loss = -(torch.stack(logps) * rets_gpu).sum()
    opt.zero_grad()
    loss.backward()
    opt.step()
    D2H += 1
    return loss.item()                                # D2H + Sync


def main():
    global dev
    print(f"rlprobe: stage={STAGE} episodes={EPISODES} pin={PIN} "
          f"nonblock={NONBLOCK} sync={SYNC} item={ITEM}")
    torch.manual_seed(0)

    # stage 0: cuInit
    assert torch.cuda.is_available(), "no CUDA card visible"
    torch.cuda.init()
    print("stage0: cuInit ok,", torch.cuda.get_device_name(0))
    if STAGE == 0:
        return
    dev = torch.device("cuda:0")

    # stage 1: context + policy onto the GPU
    pol = Policy().to(dev)
    print("stage1: policy on", dev)
    if STAGE == 1:
        return

    # stage 2/3: one episode (+ one update)
    env = CartPole(seed=0)
    logps, rewards = rollout(env, pol, train=(STAGE >= 3))
    print(f"stage2: rollout {len(rewards)} steps, return={sum(rewards):.0f}")
    if STAGE == 2:
        return

    opt = torch.optim.Adam(pol.parameters(), lr=1e-2)
    loss = update(pol, opt, logps, rewards)
    print(f"stage3: update loss={loss:.3f}")
    if STAGE == 3:
        return

    # stage 4: training, steady state
    t0 = time.monotonic()
    rets = []
    for ep in range(EPISODES):
        logps, rewards = rollout(env, pol, train=True)
        update(pol, opt, logps, rewards)
        rets.append(sum(rewards))
        if (ep + 1) % 10 == 0:
            print(f"  ep {ep + 1:4d}  return={rets[-1]:5.0f}  "
                  f"mean10={np.mean(rets[-10:]):6.1f}")
    torch.cuda.synchronize()
    dt = time.monotonic() - t0
    print(f"stage4: {EPISODES} episodes in {dt:.1f}s, "
          f"mean10={np.mean(rets[-10:]):.1f}")
    print(f"transfers: H2D={H2D} D2H={D2H} (~{(H2D + D2H) / dt:.0f}/s)")


if __name__ == "__main__":
    main()
