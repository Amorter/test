use num_traits::Float;
use std::fmt;
use std::ops;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use log::trace;

pub(crate) type Matrix<T> = ndarray::Array2<T>;

pub(crate) trait LapJVCost: Float + ops::AddAssign + ops::SubAssign + std::fmt::Debug {}
impl<T> LapJVCost for T where T: Float + ops::AddAssign + ops::SubAssign + std::fmt::Debug {}

#[derive(Debug, Copy, Clone)]
pub(crate) enum ErrorKind {
    Msg(&'static str),
    Cancelled,
}


#[derive(Debug)]
pub(crate) struct LapJVError {
    kind: ErrorKind,
}

impl LapJVError {
    pub(crate) fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl std::fmt::Display for LapJVError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self.kind {
            ErrorKind::Msg(string) => write!(f, "{}", string),
            ErrorKind::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for LapJVError {}

pub(crate) struct LapJV<'a, T: 'a> {
    costs: &'a Matrix<T>,
    dim: usize,
    free_rows: Vec<usize>,
    v: Vec<T>,
    in_col: Vec<usize>,
    in_row: Vec<usize>,
    cancellation: Cancellation,
}

/// Solve LAP problem given cost matrix
/// This is an implementation of the LAPJV algorithm described in:
/// R. Jonker, A. Volgenant. A Shortest Augmenting Path Algorithm for
/// Dense and Sparse Linear Assignment Problems. Computing 38, 325-340
/// (1987)
pub(crate) fn lapjv<T>(costs: &Matrix<T>) -> Result<(Vec<usize>, Vec<usize>), LapJVError>
where
    T: LapJVCost,
{
    LapJV::new(costs).solve()
}

/// Calculate solution cost by a result row
pub(crate) fn cost<T>(input: &Matrix<T>, row: &[usize]) -> T
where
    T: LapJVCost,
{
    (0..row.len())
        .fold(T::zero(), |acc, i| acc + input[(i, row[i])])
}

#[derive(Clone)]
pub(crate) struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Solve LAP problem given cost matrix
/// This is an implementation of the LAPJV algorithm described in:
/// R. Jonker, A. Volgenant. A Shortest Augmenting Path Algorithm for
/// Dense and Sparse Linear Assignment Problems. Computing 38, 325-340
/// (1987)
impl<'a, T> LapJV<'a, T>
where
    T: LapJVCost,
{
    pub(crate) fn new(costs: &'a Matrix<T>) -> Self {
        let dim = costs.dim().0; // square matrix dimensions
        let free_rows = Vec::with_capacity(dim); // list of unassigned rows.
        let v = Vec::with_capacity(dim);
        let in_row = vec![0; dim];
        let in_col = Vec::with_capacity(dim);
        let cancellation = Cancellation(Default::default());
        Self {
            costs,
            dim,
            free_rows,
            v,
            in_col,
            in_row,
            cancellation
        }
    }

    /// Returns a `Cancellation` token which can be cancelled from another thread.
    pub(crate) fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn check_cancelled(&self) -> Result<(), LapJVError> {
        if self.cancellation.is_cancelled() {
            return Err(LapJVError { kind: ErrorKind::Cancelled });
        }
        Ok(())
    }

    pub(crate) fn solve(mut self) -> Result<(Vec<usize>, Vec<usize>), LapJVError> {
        if self.costs.dim().0 != self.costs.dim().1 {
            return Err(LapJVError { kind: ErrorKind::Msg("Input error: matrix is not square") } );
        }
        self.ccrrt_dense();

        let mut i = 0;
        while !self.free_rows.is_empty() && i < 2 {
            self.check_cancelled()?;
            self.carr_dense();
            i += 1;
        }

        if !self.free_rows.is_empty() {
            self.ca_dense()?;
        }

        Ok((self.in_row, self.in_col))
    }

    // Column-reduction and reduction transfer for a dense cost matrix
    fn ccrrt_dense(&mut self) {
        let mut unique = vec![true; self.dim];
        let mut in_row_not_set = vec![true; self.dim];

        for row in self.costs.lanes(ndarray::Axis(0)) {
            let (min_index, min_value) = row.indexed_iter().skip(1).fold(
                (0, row[0]),
                |(old_idx, old_min), (new_idx, &new_min)| {
                    if new_min < old_min {
                        (new_idx, new_min)
                    } else {
                        (old_idx, old_min)
                    }
                },
            );
            self.in_col.push(min_index);
            self.v.push(min_value);
        }

        for j in (0..self.dim).rev() {
            let i = self.in_col[j];
            if in_row_not_set[i] {
                self.in_row[i] = j;
                in_row_not_set[i] = false;
            } else {
                unique[i] = false;
                self.in_col[j] = std::usize::MAX;
            }
        }

        for i in 0..self.dim {
            if in_row_not_set[i] {
                self.free_rows.push(i);
            } else if unique[i] {
                let j = self.in_row[i];
                let mut min = T::max_value();
                for j2 in 0..self.dim {
                    if j2 == j {
                        continue;
                    }
                    let c = self.reduced_cost(i, j2);
                    if c < min {
                        min = c;
                    }
                }
                self.v[j] -= min;
            }
        }
    }

    // Augmenting row reduction for a dense cost matrix
    fn carr_dense(&mut self) {
        // AUGMENTING ROW REDUCTION
        // scan all free rows.
        // in some cases, a free row may be replaced with another one to be scanned next.
        trace!("carr_dense");
        let dim = self.dim;
        let mut current = 0;
        let mut new_free_rows = 0; // start list of rows still free after augmenting row reduction.
        let mut rr_cnt = 0;
        let num_free_rows = self.free_rows.len();

        while current < num_free_rows {
            rr_cnt += 1;
            let free_i = self.free_rows[current];
            current += 1;
            // find minimum and second minimum reduced cost over columns.
            let (v1, v2, mut j1, j2) = find_umins_plain(self.costs.row(free_i), &self.v);

            let mut i0 = self.in_col[j1];
            let v1_new = self.v[j1] - (v2 - v1);
            let v1_lowers = v1_new < self.v[j1]; // the trick to eliminate the epsilon bug

            if rr_cnt < current * dim {
                if v1_lowers {
                    // change the reduction of the minimum column to increase the minimum
                    // reduced cost in the row to the subminimum.
                    self.v[j1] = v1_new;
                } else if i0 != std::usize::MAX && j2.is_some() {
                    // minimum and subminimum equal.
                    // minimum column j1 is assigned.
                    // swap columns j1 and j2, as j2 may be unassigned.
                    j1 = j2.unwrap();
                    i0 = self.in_col[j1];
                }
                if i0 != std::usize::MAX {
                    // minimum column j1 assigned earlier.
                    if v1_lowers {
                        // put in current k, and go back to that k.
                        // continue augmenting path i - j1 with i0.
                        current -= 1;
                        self.free_rows[current] = i0;
                    } else {
                        // no further augmenting reduction possible.
                        // store i0 in list of free rows for next phase.
                        self.free_rows[new_free_rows] = i0;
                        new_free_rows += 1;
                    }
                }
            } else if i0 != std::usize::MAX {
                self.free_rows[new_free_rows] = i0;
                new_free_rows += 1;
            }
            self.in_row[free_i] = j1;
            self.in_col[j1] = free_i;
        }
        self.free_rows.truncate(new_free_rows);
    }

    // Augment for a dense cost matrix
    fn ca_dense(&mut self) -> Result<(), LapJVError> {
        let dim = self.dim;
        let mut pred = vec![0; dim];

        let free_rows = std::mem::replace(&mut self.free_rows, vec![]);
        for freerow in free_rows {
            trace!("looking at freerow={}", freerow);

            self.check_cancelled()?;

            let mut i = std::usize::MAX;
            let mut k = 0;
            let mut j = self.find_path_dense(freerow, &mut pred);
            debug_assert!(j < dim);
            while i != freerow {
                i = pred[j];
                self.in_col[j] = i;
                std::mem::swap(&mut j, &mut self.in_row[i]);
                k += 1;
                if k > dim {
                    return Err(LapJVError { kind: ErrorKind::Msg("Error: ca_dense will not finish") });
                }
            }
        }
        Ok(())
    }

    /// Single iteration of modified Dijkstra shortest path algorithm as explained in the JV paper
    /// return The closest free column index
    fn find_path_dense(&mut self, start_i: usize, pred: &mut [usize]) -> usize {
        let dim = self.dim;
        let mut collist = Vec::with_capacity(dim); // list of columns to be scanned in various ways.
        let mut d = Vec::with_capacity(dim); // 'cost-distance' in augmenting path calculation.

        let mut lo = 0;
        let mut hi = 0;
        let mut n_ready = 0;

        // Dijkstra shortest path algorithm.
        // runs until unassigned column added to shortest path tree.
        for i in 0..dim {
            collist.push(i);
            d.push(self.reduced_cost(start_i, i));
            pred[i] = start_i;
        }

        trace!("d: {:?}", d);
        let mut final_j = None;
        while final_j.is_none() {
            if lo == hi {
                trace!("{}..{} -> find", lo, hi);
                n_ready = lo;
                hi = find_dense(dim, lo, &d, &mut collist);
                trace!("check {}..{}", lo, hi);
                // check if any of the minimum columns happens to be unassigned.
                // if so, we have an augmenting path right away.
                for &j in collist.iter().take(hi).skip(lo) {
                    if self.in_col[j] == std::usize::MAX {
                        final_j = Some(j);
                    }
                }
            }

            if final_j.is_none() {
                trace!("{}..{} -> scan", lo, hi);
                final_j = self.scan_dense(&mut lo, &mut hi, &mut d, &mut collist, pred);
            }
        }

        trace!("found final_j={:?}", final_j);
        trace!("cols={:?}", collist);
        let mind = d[collist[lo]];
        for &j in collist.iter().take(n_ready) {
            self.v[j] += d[j] - mind;
        }
        final_j.unwrap()
    }


    fn scan_dense(
        &self,
        plo: &mut usize,
        phi: &mut usize,
        d: &mut [T],
        collist: &mut [usize],
        pred: &mut [usize],
    ) -> Option<usize> {
        let mut lo = *plo;
        let mut hi = *phi;
        while lo != hi {
            let j = collist[lo];
            lo += 1;
            let i = self.in_col[j];
            let mind = d[j];
            let h = self.reduced_cost(i, j) - mind;
            for k in hi..collist.len() {
                let j = collist[k];
                let cred_ij = self.reduced_cost(i, j) - h;
                if cred_ij < d[j] {
                    d[j] = cred_ij;
                    pred[j] = i;
                    if (cred_ij - mind).abs() < T::epsilon() {
                        // if cred_ij == mind {
                        if self.in_col[j] == std::usize::MAX {
                            return Some(j);
                        }
                        collist[k] = collist[hi];
                        collist[hi] = j;
                        hi += 1;
                    }
                }
            }
        }
        // Note: only change lo and hi if the item was not found
        *plo = lo;
        *phi = hi;
        None
    }

    #[inline(always)]
    fn cost(&self, i: usize, j: usize) -> T {
        self.costs[(i, j)]
    }

    #[inline(always)]
    fn reduced_cost(&self, i: usize, j: usize) -> T {
        self.cost(i, j) - self.v[j]
    }
}

fn find_dense<T>(dim: usize, lo: usize, d: &[T], collist: &mut [usize]) -> usize
where
    T: LapJVCost,
{
    let mut hi = lo + 1;
    let mut mind = d[collist[lo]];
    for k in hi..dim {
        let j = collist[k];
        let h = d[j];
        if h <= mind {
            if h < mind {
                // new minimum.
                hi = lo; // restart list at index low.
                mind = h;
            }
            // new index with same minimum, put on undex up, and extend list.
            collist[k] = collist[hi];
            collist[hi] = j;
            hi += 1;
        }
    }
    hi
}

// Finds minimum and second minimum from a row, returns (min, second_min, min_index, second_min_index)
#[inline(always)]
fn find_umins_plain<T>(local_cost: ndarray::ArrayView1<T>, v: &[T]) -> (T, T, usize, Option<usize>)
where
    T: LapJVCost,
{
    let mut umin = local_cost[0] - v[0];
    let mut usubmin = T::max_value();
    let mut j1 = 0;
    let mut j2 = None;
    for j in 1..local_cost.dim() {
        let h = local_cost[j] - v[j];
        if h < usubmin {
            if h >= umin {
                usubmin = h;
                j2 = Some(j);
            } else {
                usubmin = umin;
                umin = h;
                j2 = Some(j1);
                j1 = j;
            }
        }
    }
    (umin, usubmin, j1, j2)
}