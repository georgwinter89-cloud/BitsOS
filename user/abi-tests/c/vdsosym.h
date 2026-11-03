/* AB-14: finds a function in a mapped vDSO the way glibc and musl do (the ELF header at AT_SYSINFO_EHDR, its
 * PT_DYNAMIC, the SysV hash table, the symbol versions), so tests can call the vDSO directly and check that the
 * loaders would find it. Used by c/vdso.c, dyn/vdso.c and host/tests/ab/progs/vdso_host.c. */
#ifndef VDSOSYM_H
#define VDSOSYM_H
#include <elf.h>
#include <stdint.h>
#include <string.h>

static uint32_t vdso_sysv_hash(const char *s) {
  uint32_t h = 0;
  for (; *s; s++) {
    h = (h << 4) + (unsigned char)*s;
    uint32_t top = h & 0xf0000000u;
    if (top) h ^= top >> 24;
    h &= ~top;
  }
  return h;
}

/* The address of `name` with version `ver` (NULL `ver`: any) in the vDSO whose ELF header is at `base`, or NULL. */
static void *vdso_sym(const void *base, const char *name, const char *ver) {
  const char *b = base;
  const Elf64_Ehdr *eh = base;
  if (!base || memcmp(eh->e_ident, ELFMAG, SELFMAG) || eh->e_ident[EI_CLASS] != ELFCLASS64) return 0;
  const Elf64_Phdr *ph = (const void *)(b + eh->e_phoff);
  intptr_t bias = 0;
  int have_load = 0;
  const Elf64_Dyn *dyn = 0;
  for (int i = 0; i < eh->e_phnum; i++) {
    if (ph[i].p_type == PT_LOAD && !have_load) {
      bias = (intptr_t)b + (intptr_t)ph[i].p_offset - (intptr_t)ph[i].p_vaddr;
      have_load = 1;
    } else if (ph[i].p_type == PT_DYNAMIC) {
      dyn = (const void *)(b + ph[i].p_offset);
    }
  }
  if (!have_load || !dyn) return 0;
  const char *str = 0;
  const Elf64_Sym *sym = 0;
  const uint32_t *hash = 0;
  const uint16_t *versym = 0;
  const Elf64_Verdef *verdef = 0;
  for (; dyn->d_tag != DT_NULL; dyn++) {
    const void *p = (const void *)(bias + (intptr_t)dyn->d_un.d_ptr);
    switch (dyn->d_tag) {
    case DT_STRTAB: str = p; break;
    case DT_SYMTAB: sym = p; break;
    case DT_HASH: hash = p; break;
    case DT_VERSYM: versym = p; break;
    case DT_VERDEF: verdef = p; break;
    }
  }
  if (!str || !sym || !hash) return 0;
  uint32_t nbucket = hash[0];
  const uint32_t *bucket = hash + 2, *chain = bucket + nbucket;
  if (!nbucket) return 0;
  for (uint32_t i = bucket[vdso_sysv_hash(name) % nbucket]; i; i = chain[i]) {
    const Elf64_Sym *s = &sym[i];
    int bind = ELF64_ST_BIND(s->st_info);
    if (ELF64_ST_TYPE(s->st_info) != STT_FUNC || s->st_shndx == SHN_UNDEF) continue;
    if (bind != STB_GLOBAL && bind != STB_WEAK) continue;
    if (strcmp(str + s->st_name, name)) continue;
    if (ver) {
      if (!versym || !verdef) return 0;
      uint16_t want = versym[i] & 0x7fff;
      const Elf64_Verdef *d = verdef;
      int ok = 0;
      for (;;) {
        if (!(d->vd_flags & VER_FLG_BASE) && d->vd_ndx == want) {
          const Elf64_Verdaux *a = (const void *)((const char *)d + d->vd_aux);
          ok = d->vd_hash == vdso_sysv_hash(ver) && !strcmp(str + a->vda_name, ver);
          break;
        }
        if (!d->vd_next) break;
        d = (const void *)((const char *)d + d->vd_next);
      }
      if (!ok) continue;
    }
    return (void *)(bias + (intptr_t)s->st_value);
  }
  return 0;
}
#endif
