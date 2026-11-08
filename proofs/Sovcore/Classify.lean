/-
Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
SPDX-License-Identifier: AGPL-3.0-or-later

Spec of the Rust `classify` function. The live core stays in Rust; these
theorems pin what the 38-day corpus is allowed to authorize — and what it is
not. Omitted auxiliary signals are `unknown`, never a measured fault.
-/

namespace Sovcore.Classify

inductive Wan where
  | down
  | up
  | unknown
  deriving DecidableEq, Repr

inductive Proc where
  | running
  | dead
  | unknown
  deriving DecidableEq, Repr

inductive Tunnel where
  | healthy (ms : Nat)
  | degraded (ms : Nat)
  | dead
  | unknown
  deriving DecidableEq, Repr

inductive Dns where
  | ok
  | failing
  | unknown
  deriving DecidableEq, Repr

/-- Reachability through the WAN. Absent from old records → `unknown`. -/
inductive WanReach where
  | ok
  | blackhole
  | unknown
  deriving DecidableEq, Repr

inductive Fault where
  | none
  | wanDown
  | singboxDead
  | nodeDead
  | nodeDegraded
  | dnsFail
  deriving DecidableEq, Repr

/-- Observation presented to `classify`. There is no `inject` field: ground
truth cannot affect the decision. -/
structure Obs where
  wan : Wan
  singbox : Proc
  tunnel : Tunnel
  dns : Dns
  wanReach : WanReach := .unknown

/-- Layers below WAN (process → path → DNS). `unknown` is not a fault. -/
def classifyRest (t : Obs) (budget : Nat) : Fault :=
  match t.singbox with
  | .dead => .singboxDead
  | .running | .unknown =>
    match t.tunnel with
    | .dead => .nodeDead
    | .degraded _ => .nodeDegraded
    | .healthy ms =>
      if ms > budget then .nodeDegraded
      else
        match t.dns with
        | .failing => .dnsFail
        | .ok | .unknown => .none
    | .unknown =>
      match t.dns with
      | .failing => .dnsFail
      | .ok | .unknown => .none

/-- Causal depth: WAN session → measured blackhole → process → path → DNS.
A measured blackhole (`wanok=0`) on a live session is `wanDown`.
Omitted `wanReach` stays `unknown` and must not invent it. -/
def classify (t : Obs) (budget : Nat) : Fault :=
  match t.wan with
  | .down => .wanDown
  | .up =>
    match t.wanReach with
    | .blackhole => .wanDown
    | .ok => classifyRest t budget
    | .unknown => classifyRest t budget
  | .unknown => classifyRest t budget

/-- A record that omitted `wanok` is not a blackhole measurement. -/
theorem omitted_wanReach_ne_blackhole : WanReach.unknown ≠ .blackhole := by
  intro h
  cases h

/-- Omitted `wanok` (the default) is not a blackhole measurement, so it cannot
authorize a WAN bounce. -/
theorem classify_unknown_wanReach_not_blackhole :
    classify
      { wan := .up, singbox := .running, tunnel := .healthy 200, dns := .ok,
        wanReach := .unknown }
      1500 = .none := by
  native_decide

/-- A measured blackhole on a live session is `wanDown`, even if the tunnel
probe still looks healthy. -/
theorem classify_blackhole_wan_up (budget : Nat) :
    classify
      { wan := .up, singbox := .running, tunnel := .healthy 200, dns := .ok,
        wanReach := .blackhole }
      budget = .wanDown := by
  simp [classify]

/-- Blackhole outranks a dead process: repair the path before the proxy. -/
theorem classify_blackhole_outranks_singbox (budget : Nat) :
    classify
      { wan := .up, singbox := .dead, tunnel := .dead, dns := .failing,
        wanReach := .blackhole }
      budget = .wanDown := by
  simp [classify]

theorem wan_down_outranks (sb : Proc) (tun : Tunnel) (dns : Dns) (r : WanReach)
    (budget : Nat) :
    classify
      { wan := .down, singbox := sb, tunnel := tun, dns := dns, wanReach := r }
      budget = .wanDown := by
  simp [classify]

/-- 38-day corpus: slower exit median 817 ms. Budget 1500 must not call that
ordinary sample `nodeDegraded`. -/
theorem slower_exit_median_not_degraded :
    classify
      { wan := .up, singbox := .running, tunnel := .healthy 817, dns := .ok }
      1500 = .none := by
  simp [classify, classifyRest]

/-- The same sample under the old 800 ms budget *is* `nodeDegraded` — that is
why the budget moved. -/
theorem old_budget_mislabels_slower_exit :
    classify
      { wan := .up, singbox := .running, tunnel := .healthy 817, dns := .ok }
      800 = .nodeDegraded := by
  simp [classify, classifyRest]

end Sovcore.Classify
