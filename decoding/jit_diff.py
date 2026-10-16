"""Differential test for the ARM to Thumb translator in common/src/jit.

Every translated block is run in Unicorn next to the original ARM code from the same random
register, flag and memory state, and the registers, flags, memory and next pc are compared.

Setup (once):  python3 -m venv /tmp/jitvenv && /tmp/jitvenv/bin/pip install unicorn capstone
Run:           cargo build -p common --example jit_dump
               /tmp/jitvenv/bin/python decoding/jit_diff.py IMAGE IMAGE_BASE [ENTRY...]

The translated code translates load and store addresses inline through a table of offsets, set
with JIT_REGIONS: "alias" (default) maps the IO, palette, VRAM, OAM, IWRAM and ROM regions a
second time at +0x40000000 and points the table there (IWRAM and EWRAM through page tables,
EWRAM pages pointing at themselves), "zero" has an all zero table so the
sequence runs but addresses do not change, "none" translates nothing.

`jit_diff.py random N` tests N random snippets, SEQ=k makes each k instructions long with mostly
one shared condition (the translator groups those).

IMAGE is a raw memory image of ARM code mapped at IMAGE_BASE (for example the IWRAM code blob
of a ROM at 0x03000000).
"""
import random
import struct
import subprocess
import sys
from collections import Counter

import capstone
from unicorn import *
from unicorn.arm_const import *

CACHE_BASE = 0x20010000
TABLE_BASE = 0x20300000
ALIAS = 0x40000000
REGIONS_MODE = __import__("os").environ.get("JIT_REGIONS", "alias")
DATA_BASE = 0x02000000
DATA_SIZE = 0x40000
EXTRA_REGIONS = [(0x08000000, 0x20000), (0x04000000, 0x1000), (0x05000000, 0x1000), (0x06000000, 0x18000), (0x07000000, 0x1000)]
REGS = [UC_ARM_REG_R0 + i for i in range(13)] + [UC_ARM_REG_SP, UC_ARM_REG_LR]
# unicorn errors that just mean execution reached an address that cannot run
EXIT_ERRORS = (8, 10, 18, 21)
EDGE = [0, 1, 2, 3, 0x7F, 0x80, 0xFF, 0x100, 0xFFFF, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF,
        0xFFFFFF80, 0x12345678, 0x80000001]


def region_table():
    # 256 offsets, then the page tables of EWRAM and IWRAM (512 entries each, odd table entries
    # point at them). EWRAM pages point at themselves, the paged lookup runs and the address is
    # unchanged. IWRAM pages point at the alias window, which repeats the 32 KiB.
    t = [0] * 256
    ew = [DATA_BASE + i * 512 for i in range(512)]
    iw = [ALIAS + 0x03000000 + (i % 64) * 512 for i in range(512)]
    if REGIONS_MODE == "alias":
        for top in range(4, 9):
            t[top] = ALIAS
        t[2] = TABLE_BASE + 0x400 | 1
        t[3] = TABLE_BASE + 0xC00 | 1
    else:
        ew = iw = [0] * 512
    return struct.pack("<%dI" % (256 + 1024), *t, *ew, *iw)


def run_dump(image, base, entries):
    table = 0 if REGIONS_MODE == "none" else TABLE_BASE
    res = subprocess.run(
        ["target/debug/examples/jit_dump", image, "%x" % base, "%x" % CACHE_BASE, "%x" % table]
        + ["%x" % e for e in entries],
        capture_output=True, text=True)
    if res.returncode:
        sys.exit("jit_dump failed:\n" + res.stderr[-1500:])
    out = res.stdout
    blocks, code, errs = [], {}, []
    for line in out.splitlines():
        parts = line.split()
        if parts[0] == "BLOCK":
            blocks.append(tuple(int(p, 16) for p in parts[1:4]))
        elif parts[0] in ("UNPATCHED", "PATCHED"):
            code[parts[0]] = bytes.fromhex(parts[1])
        elif parts[0] == "ERR":
            errs.append(line)
        elif parts[0] == "USED":
            print("code cache bytes:", parts[1])
    return blocks, code, errs


class Machine:
    def __init__(self, arm, image, base, code):
        mode = UC_MODE_ARM if arm else UC_MODE_THUMB | UC_MODE_MCLASS
        self.arm = arm
        self.mu = Uc(UC_ARCH_ARM, mode)
        if not arm:
            self.mu.ctl_set_cpu_model(UC_CPU_ARM_CORTEX_M0)
        self.mu.mem_map(DATA_BASE, DATA_SIZE)
        img_size = (len(image) + 0xFFF) & ~0xFFF
        self.mu.mem_map(base, img_size)
        self.mu.mem_write(base, image)
        maps = [(base, img_size)]
        # the rest of the GBA memory map that code might touch through absolute addresses
        for rb, rs in EXTRA_REGIONS:
            if not (base <= rb < base + img_size):
                self.mu.mem_map(rb, rs)
                maps.append((rb, rs))
        if base == 0x03000000 and img_size < 0x8000:
            self.mu.mem_map(base + img_size, 0x8000 - img_size)
            maps.append((base + img_size, 0x8000 - img_size))
        if not arm:
            self.mu.mem_map(CACHE_BASE, (len(code) + 0xFFF) & ~0xFFF)
            self.mu.mem_write(CACHE_BASE, code)
            if REGIONS_MODE != "none":
                self.mu.mem_map(TABLE_BASE, 0x2000)
                self.mu.mem_write(TABLE_BASE, region_table())
        # the translated code reaches the regions through the alias windows, the test compares
        # those with the regions of the ARM run
        self.alias = ALIAS if (not arm and REGIONS_MODE == "alias") else 0
        if self.alias:
            for rb, rs in maps:
                self.mu.mem_map(rb + ALIAS, rs)
            self.mu.mem_write(base + ALIAS, image)
        self.base, self.img_size = base, img_size
        self.stop = None
        self.slots = {}


def slots_of(code):
    s = {}
    for i in range(0, len(code) - 12, 4):
        if struct.unpack_from("<HH", code, i) == (0xDEFE, 0xBF00):
            s[CACHE_BASE + i] = struct.unpack_from("<I", code, i + 4)[0]
    return s


def set_state(m, regs, flags, mem):
    m.mu.mem_write(DATA_BASE, mem)
    for rb, blob in EXTRA_MEM:
        m.mu.mem_write(rb, blob)
        if m.alias:
            m.mu.mem_write(rb + m.alias, blob)
    # the mode first, sp and lr are banked per mode
    if m.arm:
        m.mu.reg_write(UC_ARM_REG_CPSR, 0x1F | (flags << 28))
    else:
        m.mu.reg_write(UC_ARM_REG_XPSR, (1 << 24) | (flags << 28))
    for r, v in zip(REGS, regs):
        m.mu.reg_write(r, v)


def get_state(m):
    regs = [m.mu.reg_read(r) for r in REGS]
    psr = m.mu.reg_read(UC_ARM_REG_CPSR if m.arm else UC_ARM_REG_XPSR)
    extra = b"".join(bytes(m.mu.mem_read(rb + m.alias, len(blob))) for rb, blob in EXTRA_MEM)
    return regs, psr >> 28, bytes(m.mu.mem_read(DATA_BASE, DATA_SIZE)) + extra


def run_arm(m, start, end):
    last = end - 4
    st = {"seen_last": False, "n": 0}

    def hook(uc, addr, size, ud):
        st["n"] += 1
        if st["seen_last"]:
            # the state right now is the state after the block
            st["next"] = addr
            st["state"] = get_state(m)
            uc.emu_stop()
            return
        if addr == last:
            st["seen_last"] = True
        if st["n"] > 400:
            st["next"] = -1
            uc.emu_stop()

    h = m.mu.hook_add(UC_HOOK_CODE, hook, begin=0, end=0xFFFFFFFF)
    err = None
    try:
        m.mu.emu_start(start, 0, count=2000)
    except UcError as e:
        err = e.errno
    m.mu.hook_del(h)
    if "state" in st:
        err = None  # whatever ran after the stop request does not count
    if "next" not in st:
        # jumped to something that is not an instruction, the pc is already past it
        pc = m.mu.reg_read(UC_ARM_REG_PC)
        thumb = m.mu.reg_read(UC_ARM_REG_CPSR) & 0x20
        st["next"] = None  # unreliable after an undefined instruction
        st["state"] = get_state(m)
    return st["next"], err, st["state"]


def run_thumb(m, start, slots, arm_start=None):
    st = {"next": None, "n": 0}

    def hook(uc, addr, size, ud):
        st["n"] += 1
        if addr == start & ~1 and st["n"] > 1:
            # a loop over the whole block jumps straight back, which is the exit to itself
            st["next"] = arm_start
            st["state"] = get_state(m)
            uc.emu_stop()
        elif addr in slots:
            st["next"] = slots[addr]
            st["slot"] = addr
            st["state"] = get_state(m)
            uc.emu_stop()
        elif not CACHE_BASE <= addr < CACHE_BASE + 0x100000:
            # left the translated code through a bx or pop to somewhere else
            st["next"] = addr
            st["state"] = get_state(m)
            uc.emu_stop()
        elif st["n"] > 4000:
            st["next"] = -1
            uc.emu_stop()

    h = m.mu.hook_add(UC_HOOK_CODE, hook, begin=0, end=0xFFFFFFFF)
    try:
        m.mu.emu_start(start | 1, 0, count=20000)
        err = None
    except UcError as e:
        err = e.errno
    m.mu.hook_del(h)
    if "state" in st:
        err = None
    if st["next"] is None:
        st["next"] = m.mu.reg_read(UC_ARM_REG_PC) & ~1
    if "state" not in st:
        st["state"] = get_state(m)
    return st["next"], err, st["state"]


def pointer_rich(rng, size):
    words = []
    for _ in range(size // 4):
        c = rng.random()
        if c < 0.5:
            words.append(DATA_BASE + 0x4000 + (rng.randrange(0x28000) & ~3))
        elif c < 0.75:
            words.append(rng.randrange(0, 300))
        else:
            words.append(rng.getrandbits(32))
    return struct.pack("<%dI" % len(words), *words)


def make_memories(rng, count=24):
    # words are a mix of valid pointers, small numbers and random bits, so code that follows
    # pointers loaded from memory gets somewhere
    out = []
    for _ in range(count):
        words = []
        for _ in range(DATA_SIZE // 4):
            c = rng.random()
            if c < 0.4:
                words.append(DATA_BASE + 0x4000 + (rng.randrange(0x28000) & ~3))
            elif c < 0.7:
                words.append(rng.randrange(0, 300))
            elif c < 0.8:
                words.append(rng.choice(EDGE))
            else:
                words.append(rng.getrandbits(32))
        out.append(struct.pack("<%dI" % len(words), *words))
    return out


MEMORIES = []
EXTRA_MEM = []


def random_state(rng):
    regs = []
    for i in range(15):
        c = rng.random()
        if c < 0.45:
            regs.append(DATA_BASE + 0x4000 + (rng.randrange(0x28000) & ~3))
        elif c < 0.6:
            regs.append(rng.randrange(0, 64))
        elif c < 0.85:
            regs.append(rng.choice(EDGE))
        else:
            regs.append(rng.getrandbits(32))
    # sp points well above the data pointers, translated code uses the space below sp as scratch
    regs[13] = DATA_BASE + 0x30000 + (rng.randrange(0x4000) & ~7)
    # lr is a plausible even/odd address
    if rng.random() < 0.5:
        regs[14] = rng.choice([0x08000100, 0x08000101, 0x03000100])
    mem = rng.choice(MEMORIES)
    return regs, rng.getrandbits(4), mem


def gen_instruction(rng, cond=None):
    """A random ARMv4T instruction of the kinds the translator supports, avoiding encodings that
    are architecturally unpredictable."""
    if cond is None:
        cond = rng.choice([14] * 6 + list(range(14)))
    reg = lambda: rng.randrange(15)  # no pc
    def reg_pc():
        return 15 if rng.random() < 0.04 else reg()
    kind = rng.random() * float(__import__('os').environ.get('KINDMAX', '1'))
    if kind < 0.40:  # data processing
        op = rng.randrange(16)
        s = rng.random() < 0.5
        if 8 <= op <= 11:
            s = True
        rd, rn = reg(), reg_pc()
        if rng.random() < 0.15:
            rd = rng.choice([8, 9, 10, 11, 12, 14])
        if 8 <= op <= 11:
            rd = 0  # the register field of the compares must be zero
        if op in (13, 15):
            rn = 0  # and the one of mov and mvn
        form = rng.random()
        if form < 0.3:
            op2 = (rng.randrange(16) << 8) | rng.choice([0, 1, 2, 0x7F, 0x80, 0xFF, rng.randrange(256)])
            return cond << 28 | 1 << 25 | op << 21 | s << 20 | rn << 16 | rd << 12 | op2
        rm = reg_pc() if rng.random() < 0.1 else reg()
        shift = rng.randrange(4)
        if form < 0.7:
            amount = rng.choice([0, 1, 2, 4, 8, 16, 31, rng.randrange(32)])
            op2 = amount << 7 | shift << 5 | rm
        else:
            op2 = reg() << 8 | shift << 5 | 1 << 4 | rm
        return cond << 28 | op << 21 | s << 20 | rn << 16 | rd << 12 | op2
    if kind < 0.47:  # mul / mla
        rd, rm, rs, rn = reg(), reg(), reg(), reg()
        while rm == rd:
            rm = reg()
        acc, s = rng.random() < 0.5, rng.random() < 0.5
        if not acc:
            rn = 0  # should be zero
        return cond << 28 | acc << 21 | s << 20 | rd << 16 | rn << 12 | rs << 8 | 0x90 | rm
    if kind < 0.77:  # ldr str ldrb strb
        load, byte = rng.random() < 0.5, rng.random() < 0.4
        pre, up = rng.random() < 0.7, rng.random() < 0.7
        wb = rng.random() < 0.3
        rd, rn = reg(), reg_pc() if rng.random() < 0.1 else reg()
        if (wb or not pre) and rd == rn:
            rd = (rn + 1) % 15
        if not pre:
            wb = False
        w = wb
        if rng.random() < 0.6:
            imm = rng.choice([0, 4, 8, 4 * rng.randrange(32), rng.randrange(4096), rng.randrange(40)])
            off = imm
            i = 0
        else:
            rm = reg()
            if rm == rn and (wb or not pre):
                rm = (rn + 1) % 15
            off = rng.randrange(32) << 7 | rng.randrange(4) << 5 | rm
            i = 1
        return cond << 28 | 2 << 25 | i << 25 | pre << 24 | up << 23 | byte << 22 | w << 21 | load << 20 | rn << 16 | rd << 12 | off
    if kind < 0.87:  # halfword and signed transfers
        load = rng.random() < 0.7
        sh = rng.choice([1] if not load else [1, 2, 3])
        pre, up = rng.random() < 0.7, rng.random() < 0.7
        wb = pre and rng.random() < 0.3
        rd, rn = reg(), reg()
        if (wb or not pre) and rd == rn:
            rd = (rn + 1) % 15
        if rng.random() < 0.6:
            imm = rng.choice([0, 2, 4, rng.randrange(256)])
            return cond << 28 | pre << 24 | up << 23 | 1 << 22 | wb << 21 | load << 20 | rn << 16 | rd << 12 | (imm >> 4) << 8 | 9 << 4 | sh << 5 | (imm & 0xF)
        rm = reg()
        if rm == rn and (wb or not pre):
            rm = (rn + 1) % 15
        return cond << 28 | pre << 24 | up << 23 | wb << 21 | load << 20 | rn << 16 | rd << 12 | 9 << 4 | sh << 5 | rm
    # ldm / stm
    load = rng.random() < 0.5
    pre, up = rng.random() < 0.5, rng.random() < 0.5
    rn = reg()
    if rng.random() < 0.3:
        rn = 13
    n = rng.choice([1, 2, 3, 4, 6, 8, 10])
    lst = 0
    for _ in range(n):
        lst |= 1 << rng.randrange(15 if not load else 16)
    wb = rng.random() < 0.6
    if wb and lst & (1 << rn):
        lst &= ~(1 << rn)
    if lst == 0:
        lst = 1 << ((rn + 1) % 15)
    return cond << 28 | 4 << 25 | pre << 24 | up << 23 | wb << 21 | load << 20 | rn << 16 | lst


def main():
    if sys.argv[1] == "random":
        # a snippet is SEQ random instructions and a bx lr, with SEQ > 1 most of the snippets
        # share one condition so the translator groups them
        count = int(sys.argv[2])
        seq = int(__import__("os").environ.get("SEQ", "1"))
        r = random.Random(int(__import__("os").environ.get("SEED", "1")))
        words = []
        for _ in range(count):
            shared = r.choice([14] + list(range(14))) if r.random() < 0.7 else None
            words += [gen_instruction(r, shared if seq > 1 else None) for _ in range(seq)]
            words.append(0xE12FFF1E)
        image_path = "/tmp/jit_random.img"
        open(image_path, "wb").write(struct.pack("<%dI" % len(words), *words))
        base = 0x03000000
        entries = [base + 4 * (seq + 1) * k for k in range(count)]
    else:
        image_path, base = sys.argv[1], int(sys.argv[2], 16)
        entries = [int(a, 16) for a in sys.argv[3:]] or [base]
    image = open(image_path, "rb").read()
    blocks, code, errs = run_dump(image_path, base, entries)
    print("blocks:", len(blocks), "translation errors:", len(errs))
    for e in errs[:10]:
        print("  ", e)

    unpatched = code["UNPATCHED"]
    slots = slots_of(unpatched)
    arm = Machine(True, image, base, b"")
    thumb = Machine(False, image, base, unpatched)
    md = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM)
    rng = random.Random(1234)
    MEMORIES.extend(make_memories(rng))
    # fixed random contents for the GBA IO, palette, vram, oam and the free part of iwram
    for rb, rs in EXTRA_REGIONS:
        EXTRA_MEM.append((rb, rng.randbytes(rs)))
    img_size = (len(image) + 0xFFF) & ~0xFFF
    if len(image) < img_size:
        # the rest of the last page is data the code can reach
        EXTRA_MEM.append((base + len(image), pointer_rich(rng, img_size - len(image))))
    if base == 0x03000000 and img_size < 0x8000:
        EXTRA_MEM.append((base + img_size, pointer_rich(rng, 0x8000 - img_size)))
    fails = Counter()
    kinds = __import__('collections').defaultdict(set)
    shown = 0
    trials = int(__import__("os").environ.get("TRIALS", "25"))
    skipped = Counter()
    odd = []
    compared = Counter()
    focus = __import__("os").environ.get("FOCUS")
    for (a, t, end) in blocks:
        if focus and a != int(focus, 16):
            continue
        words = [struct.unpack_from("<I", image, o - base)[0] for o in range(a, end, 4)]
        # BIOS calls need an exception vector table in the M profile model
        if any((w >> 24) & 0xF == 0xF for w in words):
            skipped["swi"] += 1
            continue
        # the states of a block do not depend on which other blocks translated
        brng = random.Random(a)
        for _ in range(trials):
            regs, flags, mem = random_state(brng)
            set_state(arm, regs, flags, mem)
            set_state(thumb, regs, flags, mem)
            anext, aerr, (ar, af, am) = run_arm(arm, a, end)
            tnext, terr, (tr, tf, tm) = run_thumb(thumb, t, slots, a)
            # running into the next block (bx, pop pc, ...) is not a fault
            if aerr in EXIT_ERRORS:
                aerr = None
            if terr in EXIT_ERRORS:
                terr = None
            if aerr or terr:
                if bool(aerr) != bool(terr):
                    skipped["arm ok, thumb faulted" if terr else "arm faulted, thumb ok"] += 1
                    odd.append((a, "arm" if aerr else "thumb", regs, aerr, terr, thumb.mu.reg_read(UC_ARM_REG_PC)))
                else:
                    skipped["both faulted"] += 1
                    if focus and skipped["both faulted"] <= 5:
                        print("both faulted: arm errno", aerr, "pc %#x" % arm.mu.reg_read(UC_ARM_REG_PC), "thumb errno", terr)
                continue
            compared[a] += 1
            problems = []
            if anext is not None and tnext is not None and anext != tnext:
                problems.append("next pc arm=%#x thumb=%#x" % (anext, tnext))
            for i in range(15):
                if ar[i] != tr[i]:
                    if i == 14 and tr[i] & 1 and (tr[i] - 1) in slots and slots[tr[i] - 1] == ar[i]:
                        continue  # bl: lr points at the slot for the return
                    problems.append("r%d arm=%#x thumb=%#x" % (i, ar[i], tr[i]))
            if (af ^ tf) & 0xF:
                problems.append("flags arm=%s thumb=%s" % (bin(af), bin(tf)))
            # translated code pushes scratch registers below sp, which is dead space for the ARM code
            lo = max(0, ar[13] - 256 - DATA_BASE)
            hi = max(0, ar[13] - DATA_BASE)
            if am[:lo] + am[hi:] != tm[:lo] + tm[hi:]:
                d = [i for i in range(0, DATA_SIZE) if am[i] != tm[i] and not lo <= i < hi]
                problems.append("memory differs at %s" % [hex(DATA_BASE + x) for x in d[:4]])
            if problems:
                fails[a] += 1
                kinds[a].update(x.split()[0] if x[0] != "r" else x.split("=")[0].split()[0] for x in problems)
                if shown < int(__import__("os").environ.get("SHOW", "12")) and fails[a] == 1:
                    shown += 1
                    print("MISMATCH block %#x:" % a)
                    for o in range(a, end, 4):
                        ins = next(md.disasm(image[o - base:o - base + 4], o), None)
                        print("    %x: %s" % (o, (ins.mnemonic + " " + ins.op_str) if ins else ".word"))
                    print("   ", "; ".join(problems))
                    print("    regs", [hex(x) for x in regs[:14]], "flags", bin(flags))
                    if focus:
                        tm_ = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_THUMB)
                        off = (t & ~1) - CACHE_BASE
                        nxt = min([x for x in [b[1] & ~1 for b in blocks] if x > (t & ~1)] or [len(unpatched)+CACHE_BASE]) - CACHE_BASE
                        for ins in tm_.disasm(unpatched[off:nxt], t & ~1):
                            print("      T %x: %s %s" % (ins.address, ins.mnemonic, ins.op_str))
                        print("    arm regs  ", [hex(x) for x in ar])
                        print("    thumb regs", [hex(x) for x in tr])
    print("skipped:", dict(skipped))
    if focus:
        print("compared trials:", sum(compared.values()))
    untested = [hex(a) for (a, t, e) in blocks if not compared[a]]
    print("blocks never compared:", len(untested), untested[:40])
    seen_odd = set()
    for a, side, regs, aerr, terr, tpc in odd:
        if (a, side) not in seen_odd and len(seen_odd) < 6:
            seen_odd.add((a, side))
            print("one-sided fault in block %#x (%s faulted) errno arm=%s thumb=%s thumb pc=%#x" % (a, side, aerr, terr, tpc))
            end_ = [e for (aa, t, e) in blocks if aa == a][0]
            for o in range(a, end_, 4):
                ins = next(md.disasm(image[o - base:o - base + 4], o), None)
                print("    %x: %s" % (o, (ins.mnemonic + " " + ins.op_str) if ins else ".word"))
    print("blocks with mismatches: %d of %d" % (len(fails), len(blocks)))
    out = __import__("os").environ.get("FAILS_OUT")
    if out:
        with open(out, "w") as f:
            for a in sorted(fails):
                f.write("%x %s\n" % (a, ",".join(sorted(kinds[a]))))


main()
