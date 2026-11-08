/-
Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
SPDX-License-Identifier: AGPL-3.0-or-later

Majority vote as implemented by `sovml` random forest: count per class,
pick the max, ties keep the **lowest** class index (`n > best_n`, scan
from 0). This is not a Mixture-of-Experts: there is no gate, no softmax,
no per-sample expert routing.
-/

namespace Sovcore.Vote

/-- Recurse over remaining votes; `i` is the class index of the head. -/
def majority.go (i best bestN : Nat) : List Nat → Nat
  | [] => best
  | n :: rest =>
    if n > bestN then majority.go (i + 1) i n rest
    else majority.go (i + 1) best bestN rest

/-- Class index chosen by majority vote. Empty input is class 0 (same as a
missing feature reading 0 in `DecisionTree::predict`). -/
def majority : List Nat → Nat
  | [] => 0
  | n :: rest => majority.go 1 0 n rest

theorem empty_is_class_zero : majority [] = 0 := rfl

theorem unique_max : majority [1, 5, 2] = 1 := rfl

/-- Tie keeps the lower index — the Rust loop only updates on a strict `>`. -/
theorem tie_keeps_lowest_index : majority [3, 3] = 0 := rfl

theorem later_class_wins_only_with_more_votes : majority [2, 2, 3] = 2 := rfl

end Sovcore.Vote
