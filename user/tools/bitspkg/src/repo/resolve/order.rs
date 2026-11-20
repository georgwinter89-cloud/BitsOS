//! The install order: pacman/lib/libalpm/deps.c:213 _alpm_sortbydeps (reverse = 0) over the closure in the order
//! it was resolved. A vertex's children are the closure's packages that satisfy one of its dependencies
//! (deps.c:74 _alpm_pkg_depends_on), in closure order; a depth-first walk emits a package after its children.
//! A cycle is not an error (deps.c:161 only logs it): one package of the cycle comes before its dependency.

#[derive(Clone, Copy, PartialEq)]
enum State {
    New,
    Busy,
    Done,
}

/// `children[i]`: the vertices vertex i depends on, ascending. Returns the vertices in install order and the
/// number of cycle edges met.
pub fn sort(children: &[Vec<usize>]) -> (Vec<usize>, usize) {
    let n = children.len();
    let mut out = Vec::with_capacity(n);
    if n == 0 {
        return (out, 0);
    }
    let mut state = vec![State::New; n];
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut next = vec![0usize; n];
    let mut cycles = 0;
    let mut top = 0;
    let mut v = 0;
    loop {
        state[v] = State::Busy;
        let mut down = false;
        while next[v] < children[v].len() && !down {
            let c = children[v][next[v]];
            next[v] += 1;
            match state[c] {
                State::New => {
                    down = true;
                    parent[c] = Some(v);
                    v = c;
                }
                State::Busy => {
                    if c != v {
                        cycles += 1;
                    }
                }
                State::Done => {}
            }
        }
        if down {
            continue;
        }
        out.push(v);
        state[v] = State::Done;
        match parent[v] {
            Some(p) => v = p,
            None => {
                top += 1;
                while top < n && state[top] != State::New {
                    top += 1;
                }
                if top >= n {
                    break;
                }
                v = top;
            }
        }
    }
    (out, cycles)
}
