# Completions for rip. Written by hand: bash's own completion generators
# have nothing built in for a first word that is sometimes a subcommand and
# sometimes a file, and rip mixes the two (docs/design.md §2.1). This file
# is self-contained: it does not call _init_completion or _filedir, so it
# works whether it is sourced directly or lazy-loaded by the bash-completion
# package from share/bash-completion/completions/.

# Real readline splits COMP_WORDS not just on whitespace but at every run of
# a non-whitespace COMP_WORDBREAKS character (default includes `=` and `:`),
# so `--config=c.toml` arrives as three words (`--config`, `=`, `c.toml`)
# and a colon-bearing path splits the same way. Glues any such run back
# onto its neighbors into $_rip_words/$_rip_cword, so the rest of this file
# can work with one word per rip argument again, the same as src/main.rs's
# own argv. A whitespace-separated argument that happens to be made up
# entirely of '=' and/or ':' (e.g. a file actually named "=") is misglued to
# its neighbor; a real one never is.
_rip_reassemble() {
    _rip_words=("${COMP_WORDS[0]}")
    _rip_cword=0
    local i w glue=0
    for ((i = 1; i <= COMP_CWORD; i++)); do
        w=${COMP_WORDS[i]}
        if [[ -n $w && $w != *[^=:]* ]]; then
            _rip_words[_rip_cword]+=$w
            glue=1
        elif ((glue)); then
            _rip_words[_rip_cword]+=$w
            glue=0
        else
            _rip_cword=$((_rip_cword + 1))
            _rip_words[_rip_cword]=$w
        fi
    done
}

# Sets $_rip_state from the words before the cursor: "first" (no word yet),
# "files" (after a file or an rm-style flag), "--" (after --), or a
# subcommand name. Mirrors src/main.rs's first_word(). Reads $_rip_words/
# $_rip_cword (_rip_reassemble), not COMP_WORDS/COMP_CWORD directly, so
# --config=PATH is one word here exactly as first_word() sees it.
_rip_state() {
    local i=1 w
    _rip_state=first
    while ((i < _rip_cword)); do
        w=${_rip_words[i]}
        case $w in
        --)
            _rip_state=--
            return
            ;;
        --config | --completions)
            i=$((i + 1))
            ;;
        --config=* | --completions=* | -h | --help | -V | --version)
            # inline value, or a flag the parser leaves state unchanged for
            # (src/main.rs first_word()): keep scanning.
            ;;
        -*)
            # An rm-style flag before a subcommand is a parse error
            # (src/main.rs first_word()), so from here only files make sense.
            _rip_state=files
            return
            ;;
        *)
            case " undo list restore empty purge " in
            *" $w "*) _rip_state=$w ;;
            *) _rip_state=files ;;
            esac
            return
            ;;
        esac
        i=$((i + 1))
    done
}

# Strips the quoting/escaping readline leaves in place on a still-typed
# prefix (an inserted `My\ Doc`, or a still-open `'My Do`), so it can be
# compared against real (unescaped) file and trashed-path names; compgen -f
# has the same problem, since it matches its "$cur" argument literally, not
# shell-unescaped. Handles only rip's own two forms of an in-progress shell
# word -- backslash escapes and one open quote -- not a full shell parse,
# and never evals the input.
_rip_dequote() {
    local s=$1
    case $s in
    \'*)
        s=${s#\'}
        s=${s//\'/}
        ;;
    \"*)
        s=${s#\"}
        s=${s//\\\"/\"}
        s=${s//\\\\/\\}
        ;;
    *)
        s=${s//\\/}
        ;;
    esac
    printf '%s' "$s"
}

# How many characters, counting from the end of $1 (bash's own un-glued
# $cur, i.e. COMP_WORDS[COMP_CWORD]), readline will actually replace before
# inserting a match: 0 when $1 is itself only a run of '='/':', since
# readline appends after such a run instead of replacing it (verified
# interactively: after `--config=`, the `=` stays and the match is inserted
# right after it); otherwise all of $1.
_rip_replace_len() {
    local s=$1
    if [[ -z $s || $s != *[^=:]* ]]; then
        printf 0
    else
        printf '%s' "${#s}"
    fi
}

# Appends compgen's matches to COMPREPLY one per line, so a candidate with a
# space (a file, or a trashed path by way of _rip_trashed) is not split by
# word-splitting the way an unquoted `COMPREPLY+=($(compgen ...))` would
# split it.
_rip_add() {
    local -a matches
    mapfile -t matches < <(compgen "$@")
    COMPREPLY+=("${matches[@]}")
}

# Completes a file/directory argument. $1 is the raw prefix typed so far,
# reassembled from possibly several COMP_WORDS entries (a colon-bearing
# path, or --config=PATH's value with the flag already stripped by the
# caller); $2 is bash's own un-glued $cur. $1 is dequoted before matching
# (_rip_dequote); each match is then trimmed back to what _rip_replace_len
# says readline will actually keep on the line, since readline replaces
# starting after its own (COMP_WORDBREAKS-based) word start, not $1's --
# the bash-completion __ltrim_colon_completions trick, generalized to both
# of rip's split characters and to the dequoting above.
_rip_add_files() {
    local prefix trim cur_len
    prefix=$(_rip_dequote "$1")
    cur_len=$(_rip_replace_len "$2")
    trim=$((${#1} - cur_len))
    local -a matches
    mapfile -t matches < <(compgen -f -- "$prefix")
    local m
    for m in "${matches[@]}"; do
        COMPREPLY+=("${m:trim}")
    done
}

# Trashed original paths under the current directory, filtered by the
# dequoted, reassembled prefix ($1; see _rip_add_files) and added to
# COMPREPLY trimmed the same way. `read -d ''` splits only on NUL, so a
# record's own embedded newline (from a hostile or oddly named trashed
# path) stays part of it; such a record is then skipped whole rather than
# split into extra candidates (docs/design.md §2.1).
_rip_trashed() {
    local prefix trim cur_len
    prefix=$(_rip_dequote "$1")
    cur_len=$(_rip_replace_len "$2")
    trim=$((${#1} - cur_len))
    local rec path
    while IFS= read -r -d '' rec; do
        path=${rec#*$'\t'}
        [[ $path == *$'\n'* ]] && continue
        [[ $path == "$prefix"* ]] && COMPREPLY+=("${path:trim}")
    done < <(rip list -0 2>/dev/null)
}

_rip_root_flags() {
    _rip_add -W '-f --force -i -I -v --verbose -r -R --recursive -d --dir' -- "$1"
}

_rip() {
    COMPREPLY=()
    # Filename-style quoting for any candidate with a space or shell
    # metacharacter (files and trashed paths alike); a no-op outside real
    # readline completion (e.g. this file's own tests), where compopt
    # always fails.
    compopt -o filenames 2>/dev/null

    _rip_reassemble
    _rip_state
    local state=$_rip_state

    local cur_raw=${COMP_WORDS[COMP_CWORD]}
    local prev_raw=${COMP_WORDS[COMP_CWORD - 1]}
    local full_raw=${_rip_words[_rip_cword]}

    # --config's value: either split form, `--config PATH` (prev_raw is the
    # flag itself) or `--config=PATH` (the flag and value share one glued
    # word). --completions is deliberately not offered here, matching the
    # subcommand/flag lists below: its value is one of bash/zsh/fish, not a
    # file.
    if [[ $state != -- ]]; then
        if [[ $full_raw == --config=* ]]; then
            _rip_add_files "${full_raw#--config=}" "$cur_raw"
            return
        fi
        if [[ $prev_raw == --config ]]; then
            _rip_add_files "$full_raw" "$cur_raw"
            return
        fi
    fi
    [[ $state != -- ]] && _rip_add -W '--config' -- "$cur_raw"

    case $state in
    first)
        _rip_add -W 'undo list restore empty purge -h --help -V --version' -- "$cur_raw"
        _rip_root_flags "$cur_raw"
        _rip_add_files "$full_raw" "$cur_raw"
        ;;
    files)
        _rip_root_flags "$cur_raw"
        _rip_add_files "$full_raw" "$cur_raw"
        ;;
    --)
        _rip_add_files "$full_raw" "$cur_raw"
        ;;
    undo)
        _rip_add -W '-y --yes' -- "$cur_raw"
        ;;
    list)
        _rip_add -W '-a --all -0 --null' -- "$cur_raw"
        ;;
    restore)
        _rip_trashed "$full_raw" "$cur_raw"
        _rip_add -W '-a --all --rename -y --yes' -- "$cur_raw"
        ;;
    empty)
        _rip_add -W '--older-than --max-size -y --yes' -- "$cur_raw"
        ;;
    purge)
        _rip_trashed "$full_raw" "$cur_raw"
        _rip_add -W '-a --all -y --yes' -- "$cur_raw"
        ;;
    esac
}

complete -F _rip rip
