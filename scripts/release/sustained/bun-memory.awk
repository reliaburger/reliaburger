# Bun's memory by kind (#1417), for the soak's guest inventory. Bun uses
# glibc malloc, which exports no statistics, so this reads what the kernel
# shows instead:
#
#   awk -f bun-memory.awk /proc/PID/status /proc/PID/smaps_rollup /proc/PID/smaps
#
# and prints `bun_mem_<measure> <value>` lines (sizes in KiB):
#
#   rss_kb, anonymous_kb, private_dirty_kb, swap_kb   from smaps_rollup
#   threads                                           from status
#   heap_kb                  the main arena, `[heap]`
#   arena_kb, arenas         glibc thread arenas: their RSS and how many
#                            64 MiB arena heaps hold it
#   stack_kb, stacks         thread stacks
#   other_anon_kb            every other anonymous mapping (large mallocs
#                            get their own mmap and land here)
#   file_kb                  file-backed mappings
#   top START RSS_KB KIND    the five anonymous mappings with the most RSS
#
# Arenas: on 64-bit glibc a thread arena's heap is a 64 MiB (HEAP_MAX_SIZE)
# reservation aligned to 64 MiB. glibc maps it PROT_NONE and makes the part
# in use read-write, so smaps shows an anonymous rw mapping starting on a
# 64 MiB boundary, followed by a `---p` reserve ending on the next one. A
# mapping counts as an arena heap when it starts 64 MiB-aligned and either
# is exactly 64 MiB or is followed directly by an anonymous `---p` mapping
# that ends 64 MiB-aligned. A glibc that names its mappings
# (glibc.mem.decorate_maps) says `malloc arena`, which counts too. If the
# kernel merges an arena with an anonymous neighbour below it, the merged
# mapping doesn't start aligned and counts as other anonymous memory.
#
# Stacks: `[stack]` (the main thread) and, for other threads, an anonymous
# rw mapping directly after an anonymous `---p` guard of 64 KiB or less;
# glibc puts a thread's guard page below its stack. A guard ending on a
# 64 MiB boundary is taken for an arena reserve, not a guard. Named
# `pthread stack` mappings count too.
#
# One pass, POSIX awk only (the guests run mawk): addresses stay hex
# strings, compared as text, and alignment is read off the last digits.

BEGIN {
    for (i = 1; i <= 5; i++) { top_rss[i] = 0; top_start[i] = ""; top_kind[i] = "" }
    have_current = 0
}

# The low 26 bits of a hex address are zero: the last six digits, and the
# low two bits of the seventh.
function aligned(address,   digits) {
    digits = address
    while (length(digits) < 8) digits = "0" digits
    return substr(digits, length(digits) - 5) == "000000" && substr(digits, length(digits) - 6, 1) ~ /^[048c]$/
}

function remember(start, rss, kind,   i) {
    if (rss <= top_rss[5]) return
    i = 5
    while (i > 1 && rss > top_rss[i - 1]) {
        top_rss[i] = top_rss[i - 1]; top_start[i] = top_start[i - 1]; top_kind[i] = top_kind[i - 1]
        i--
    }
    top_rss[i] = rss; top_start[i] = start; top_kind[i] = kind
}

# Classifies the previous mapping (p_*), now that the one before it (b_*)
# and the header of the one after it (n_*) are known.
function classify(   kind) {
    if (substr(p_name, 1, 1) == "/") { file_kb += p_rss; return }
    kind = "other"
    if (p_name == "[heap]") kind = "heap"
    else if (p_name ~ /malloc arena/) kind = "arena"
    else if (p_name == "[stack]" || p_name ~ /pthread stack/) kind = "stack"
    else if (p_name == "" && p_perms !~ /^---/) {
        if (aligned(p_start) && (p_size == 65536 || (n_name == "" && n_perms ~ /^---/ && n_start == p_end && aligned(n_end))))
            kind = "arena"
        else if (b_name == "" && b_perms ~ /^---/ && b_end == p_start && b_size <= 64 && !aligned(b_end))
            kind = "stack"
    }
    if (kind == "heap") heap_kb += p_rss
    else if (kind == "arena") { arena_kb += p_rss; arenas++ }
    else if (kind == "stack") { stack_kb += p_rss; stacks++ }
    else other_anon_kb += p_rss
    remember(p_start, p_rss, kind)
}

function shift_mappings() {
    b_start = p_start; b_end = p_end; b_perms = p_perms; b_name = p_name; b_size = p_size; b_rss = p_rss
    p_start = c_start; p_end = c_end; p_perms = c_perms; p_name = c_name; p_size = c_size; p_rss = c_rss
    classify()
}

FILENAME ~ /status$/ && $1 == "Threads:" { threads = $2 + 0 }

FILENAME ~ /smaps_rollup$/ {
    if ($1 == "Rss:") rss_kb = $2 + 0
    else if ($1 == "Anonymous:") anonymous_kb = $2 + 0
    else if ($1 == "Private_Dirty:") private_dirty_kb = $2 + 0
    else if ($1 == "Swap:") swap_kb = $2 + 0
}

FILENAME ~ /smaps$/ && $1 ~ /^[0-9a-f]+-[0-9a-f]+$/ {
    dash = index($1, "-")
    n_start = substr($1, 1, dash - 1); n_end = substr($1, dash + 1); n_perms = $2
    n_name = ""
    for (i = 6; i <= NF; i++) n_name = n_name (i > 6 ? " " : "") $i
    if (have_current) shift_mappings()
    c_start = n_start; c_end = n_end; c_perms = n_perms; c_name = n_name; c_size = 0; c_rss = 0
    have_current = 1
    next
}

FILENAME ~ /smaps$/ && $1 == "Size:" { c_size = $2 + 0 }
FILENAME ~ /smaps$/ && $1 == "Rss:" { c_rss = $2 + 0 }

END {
    n_start = ""; n_end = ""; n_perms = ""; n_name = "-"
    if (have_current) shift_mappings()
    print "bun_mem_rss_kb " rss_kb + 0
    print "bun_mem_anonymous_kb " anonymous_kb + 0
    print "bun_mem_private_dirty_kb " private_dirty_kb + 0
    print "bun_mem_swap_kb " swap_kb + 0
    print "bun_mem_threads " threads + 0
    print "bun_mem_heap_kb " heap_kb + 0
    print "bun_mem_arena_kb " arena_kb + 0
    print "bun_mem_arenas " arenas + 0
    print "bun_mem_stack_kb " stack_kb + 0
    print "bun_mem_stacks " stacks + 0
    print "bun_mem_other_anon_kb " other_anon_kb + 0
    print "bun_mem_file_kb " file_kb + 0
    for (i = 1; i <= 5; i++)
        if (top_rss[i] > 0) print "bun_mem_top " top_start[i] " " top_rss[i] " " top_kind[i]
}
