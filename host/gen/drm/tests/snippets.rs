//! The parser on small pieces of C with the constructs the real headers use.

use drm_gen::eval::{self, CTy};
use drm_gen::parse::{self, Kind, Ty};
use drm_gen::pp::{Input, Pp, Tok};

const SRC: &str = r#"/* notice */
#ifndef G_
#define G_
#if defined(__linux__)
typedef unsigned int handle_t;
#else
typedef unsigned long handle_t;
#endif
#ifdef __cplusplus
#error never
#endif
#define BASE 'd'
#define CMD 0x40
#define IOWR(nr, type) _IOWR(BASE, nr, type)
#define NEG -512
#define WIDE (1ULL << 40)
#define LEN 4
enum e { A = NEG, B, C = 1 << 2 };
struct s {
	__u32 a;
	__u64 b;
	char name[LEN];
	union {
		struct { __u32 x; __u32 y; } pt;
		__u64 raw;
	};
	struct { handle_t h; char *p; } list[2];
	enum e kind;
};
union u { struct s in; __u32 out; };
#define IOCTL_S IOWR(CMD + 1, union u)
#endif
"#;

fn val(pp: &Pp, m: &parse::Model, name: &str) -> eval::Done {
    eval::eval(pp, m, &[Tok::Id(name.into())]).unwrap()
}

#[test]
fn constructs_of_the_headers() {
    let pp = Pp::run(&[Input { label: "x/t.h", text: SRC }]).unwrap();
    assert!(pp.notices[0].contains("notice"));
    let m = parse::parse(&pp).unwrap();
    let s = m.tag("struct", "s").unwrap();
    let a = &m.aggs[s];
    // a@0, b@8, name@16, anon union@24 (8), list@32 (2 x 16), kind@64, size 72
    let offs: Vec<u64> = a.members.iter().map(|mb| mb.offset).collect();
    assert_eq!(offs, [0, 8, 16, 24, 32, 64]);
    assert_eq!((a.size, a.align), (72, 8));
    let Ty::Agg(anon) = a.members[3].ty else { panic!() };
    assert_eq!(m.aggs[anon].kind, Kind::Anon { parent: s, k: 0 });
    assert_eq!(m.aggs[anon].rust, "s__anon0");
    assert!(m.aggs.iter().any(|g| g.rust == "s__pt" && matches!(&g.kind, Kind::Inline { path, .. } if path == "pt")));
    assert!(m.aggs.iter().any(|g| g.rust == "s__list" && matches!(&g.kind, Kind::Inline { path, .. } if path == "list[0]")));
    let u = m.tag("union", "u").unwrap();
    assert_eq!(m.aggs[u].size, 72);
    let io = val(&pp, &m, "IOCTL_S");
    assert_eq!(io.val.v, 0xc048_6441);
    assert_eq!(io.ioctl_ty.unwrap().len(), 2);
    assert_eq!(val(&pp, &m, "NEG").val.v, -512);
    assert_eq!(val(&pp, &m, "WIDE").val.t, CTy::ULong);
    assert_eq!(val(&pp, &m, "B").val.v, -511);
    assert_eq!(m.typedefs[0].ty, Ty::Prim(parse::Prim::U32));
}

#[test]
fn refuses_what_it_cannot_lay_out() {
    let flex = "struct f { __u32 n; __u32 v[]; };";
    let pp = Pp::run(&[Input { label: "f.h", text: flex }]).unwrap();
    assert!(parse::parse(&pp).is_err());
    let bitfield = "struct b { __u32 a : 3; };";
    let pp = Pp::run(&[Input { label: "b.h", text: bitfield }]).unwrap();
    assert!(parse::parse(&pp).is_err());
}
