//! Global minimum cut (Stoer–Wagner) — a graph-partition primitive.
//!
//! The cheapest way to split a weighted graph into two non-empty parts: the set
//! of edges whose total weight is smallest among all such splits. It is the
//! cross-domain optimization core that bridges into security (the *minimal* set
//! of authority edges to cut to isolate a compromised domain — quarantine with
//! the least collateral), resource placement (where to cut a dataflow across
//! GPUs so the least data crosses), and clustering.
//!
//! Like every primitive here it is **dumb mechanism, zero policy**: it computes a
//! cut weight (and, on request, which side each vertex lands on) over a
//! fixed-capacity weighted undirected graph. *What* the vertices and weights mean
//! — domains and delegated authority, kernels and buffer bytes — is the
//! orchestrator's composition.
//!
//! `no_std`, allocation-free (an `N×N` weight matrix via const generics), no
//! locks. Stoer–Wagner runs in `O(V³)`; keep `N` modest (a control-plane
//! analysis, never a hot path).

/// A weighted undirected graph of up to `N` vertices, for global min-cut.
#[derive(Clone)]
pub struct MinCutGraph<const N: usize> {
    w: [[u64; N]; N],
    n: usize,
}

impl<const N: usize> MinCutGraph<N> {
    /// A graph with `n` vertices (`0..n`) and no edges. Panics in debug if `n > N`.
    pub fn new(n: usize) -> Self {
        debug_assert!(n <= N);
        Self {
            w: [[0; N]; N],
            n: if n <= N { n } else { N },
        }
    }

    /// Number of vertices.
    pub const fn vertices(&self) -> usize {
        self.n
    }

    /// Add `weight` to the undirected edge `{a, b}` (parallel edges accumulate).
    /// Self-loops and out-of-range endpoints are ignored.
    pub fn add_edge(&mut self, a: usize, b: usize, weight: u64) {
        if a == b || a >= self.n || b >= self.n {
            return;
        }
        self.w[a][b] = self.w[a][b].saturating_add(weight);
        self.w[b][a] = self.w[b][a].saturating_add(weight);
    }

    /// The weight of the global minimum cut. Returns 0 for a graph with fewer than
    /// two vertices (nothing to separate).
    pub fn min_cut(&self) -> u64 {
        self.min_cut_inner(None)
    }

    /// The global minimum cut, also recording into `side` which partition each
    /// vertex lands on (`true` = the lighter "cut-off" side). `side` must be at
    /// least `vertices()` long; extra entries are left untouched.
    pub fn min_cut_partition(&self, side: &mut [bool]) -> u64 {
        for s in side.iter_mut() {
            *s = false;
        }
        self.min_cut_inner(Some(side))
    }

    fn min_cut_inner(&self, mut side: Option<&mut [bool]>) -> u64 {
        let n = self.n;
        if n < 2 {
            return 0;
        }

        // Working copy: merges accumulate weights into a surviving vertex.
        let mut w = self.w;
        // `group[v]` lists the original vertices merged into surviving vertex `v`.
        // Tracked as a parent-less membership: members[v] is a bitset would need
        // alloc; instead keep a per-vertex "merged-away" flag and, when partition
        // output is wanted, the set of originals folded into the phase's last
        // vertex via `co[v]` chains.
        let mut merged = [false; N];
        // co[v] = next original vertex in v's merged chain, or usize::MAX.
        let mut co_next = [usize::MAX; N];
        // co_head[v] = first original in v's chain (== v initially).
        let co_head: [usize; N] = core::array::from_fn(|i| i);

        let mut best = u64::MAX;
        let mut active = n;

        while active > 1 {
            let mut in_a = [false; N];
            let mut conn = [0u64; N];
            let mut prev = usize::MAX;
            let mut last = usize::MAX;
            let mut cut_of_phase = 0u64;

            for i in 0..active {
                // Most tightly connected vertex not yet in A.
                let mut sel = usize::MAX;
                for v in 0..n {
                    if !merged[v] && !in_a[v] && (sel == usize::MAX || conn[v] > conn[sel]) {
                        sel = v;
                    }
                }
                if sel == usize::MAX {
                    break;
                }
                if i == active - 1 {
                    cut_of_phase = conn[sel];
                }
                in_a[sel] = true;
                prev = last;
                last = sel;
                for v in 0..n {
                    if !merged[v] && !in_a[v] {
                        conn[v] = conn[v].saturating_add(w[sel][v]);
                    }
                }
            }

            if cut_of_phase < best {
                best = cut_of_phase;
                if let Some(side) = side.as_deref_mut() {
                    // The cut-of-the-phase isolates `last` (and everything merged
                    // into it) from the rest. Record that side.
                    for s in side.iter_mut().take(n) {
                        *s = false;
                    }
                    let mut cur = co_head[last];
                    while cur != usize::MAX {
                        if cur < side.len() {
                            side[cur] = true;
                        }
                        cur = co_next[cur];
                    }
                }
            }

            // Merge `last` into `prev`.
            if prev != usize::MAX && last != usize::MAX {
                merged[last] = true;
                // Indexes row `prev`/`last` and column `prev`/`last` of `w` by the
                // same `v`; the matrix layout makes an iterator no clearer.
                #[allow(clippy::needless_range_loop)]
                for v in 0..n {
                    w[prev][v] = w[prev][v].saturating_add(w[last][v]);
                    w[v][prev] = w[v][prev].saturating_add(w[v][last]);
                }
                // Splice last's original-vertex chain onto prev's.
                let mut tail = co_head[prev];
                while co_next[tail] != usize::MAX {
                    tail = co_next[tail];
                }
                co_next[tail] = co_head[last];
                active -= 1;
            } else {
                break;
            }
        }

        if best == u64::MAX {
            0
        } else {
            best
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_edge_cut_is_its_weight() {
        let mut g: MinCutGraph<2> = MinCutGraph::new(2);
        g.add_edge(0, 1, 7);
        assert_eq!(g.min_cut(), 7);
    }

    #[test]
    fn disconnected_graph_has_zero_cut() {
        let mut g: MinCutGraph<4> = MinCutGraph::new(4);
        g.add_edge(0, 1, 5);
        // vertices 2,3 are isolated -> cutting them off costs nothing.
        assert_eq!(g.min_cut(), 0);
    }

    #[test]
    fn fewer_than_two_vertices_is_zero() {
        assert_eq!(MinCutGraph::<4>::new(1).min_cut(), 0);
        assert_eq!(MinCutGraph::<4>::new(0).min_cut(), 0);
    }

    #[test]
    fn classic_stoer_wagner_example_is_four() {
        // The 8-vertex graph from Stoer & Wagner (1997); global min cut = 4.
        let mut g: MinCutGraph<8> = MinCutGraph::new(8);
        for &(a, b, c) in &[
            (0, 1, 2),
            (0, 4, 3),
            (1, 2, 3),
            (1, 4, 2),
            (1, 5, 2),
            (2, 3, 4),
            (2, 6, 2),
            (3, 6, 2),
            (3, 7, 2),
            (4, 5, 3),
            (5, 6, 1),
            (6, 7, 3),
        ] {
            g.add_edge(a, b, c);
        }
        assert_eq!(g.min_cut(), 4);
    }

    #[test]
    fn partition_isolates_a_cheaply_cut_vertex() {
        // A triangle {0,1,2} densely tied, vertex 3 hangs off 2 by a single light
        // edge — the min cut (weight 1) must put 3 alone on one side.
        let mut g: MinCutGraph<4> = MinCutGraph::new(4);
        g.add_edge(0, 1, 5);
        g.add_edge(1, 2, 5);
        g.add_edge(0, 2, 5);
        g.add_edge(2, 3, 1);
        let mut side = [false; 4];
        assert_eq!(g.min_cut_partition(&mut side), 1);
        // Exactly one of {3} / {0,1,2} is the cut-off side; vertex 3 is alone.
        let cut_off = side[3];
        assert_ne!(side[0], cut_off);
        assert_ne!(side[1], cut_off);
        assert_ne!(side[2], cut_off);
    }
}
