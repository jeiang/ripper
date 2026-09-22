# Completions for rip. Written by hand: bash's own completion generators
# have nothing built in for a first word that is sometimes a subcommand and
# sometimes a file, and rip mixes the two (docs/design.md §2.1). This file
# is self-contained: it does not call _init_completion or _filedir, so it
# works whether it is sourced directly or lazy-loaded by the bash-completion
# package from share/bash-completion/completions/.

# Sets $_rip_state from the words before the cursor: "first" (no word yet),
# "files" (after a file or an rm-style flag), "--" (after --), or a
# subcommand name. Mirrors src/main.rs's first_word().
_rip_state() {
    local i=1 w
    _rip_state=first
    while ((i < COMP_CWORD)); do
        w=${COMP_WORDS[i]}
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

# Appends compgen's matches to COMPREPLY one per line, so a candidate with a
# space (a file, or a trashed path by way of _rip_trashed) is not split by
# word-splitting the way an unquoted `COMPREPLY+=($(compgen ...))` would
# split it.
_rip_add() {
    local -a matches
    mapfile -t matches < <(compgen "$@")
    COMPREPLY+=("${matches[@]}")
}

# Trashed original paths under the current directory, filtered by $1 and
# added to COMPREPLY. `read -d ''` splits only on NUL, so a record's own
# embedded newline (from a hostile or oddly named trashed path) stays part
# of it; such a record is then skipped whole rather than split into extra
# candidates (docs/design.md §2.1).
_rip_trashed() {
    local cur=$1 rec path
    while IFS= read -r -d '' rec; do
        path=${rec#*$'\t'}
        [[ $path == *$'\n'* ]] && continue
        [[ $path == "$cur"* ]] && COMPREPLY+=("$path")
    done < <(rip list -0 2>/dev/null)
}

_rip_root_flags() {
    _rip_add -W '-f --force -i -I -v --verbose -r -R --recursive -d --dir' -- "$1"
}

_rip() {
    local cur=${COMP_WORDS[COMP_CWORD]} prev=${COMP_WORDS[COMP_CWORD - 1]}
    COMPREPLY=()
    # Filename-style quoting for any candidate with a space or shell
    # metacharacter (files and trashed paths alike); a no-op outside real
    # readline completion (e.g. this file's own tests), where compopt
    # always fails.
    compopt -o filenames 2>/dev/null

    _rip_state
    local state=$_rip_state

    if [[ $state != -- && $prev == --config ]]; then
        _rip_add -f -- "$cur"
        return
    fi
    [[ $state != -- ]] && _rip_add -W '--config' -- "$cur"

    case $state in
    first)
        _rip_add -W 'undo list restore empty purge -h --help -V --version' -- "$cur"
        _rip_root_flags "$cur"
        _rip_add -f -- "$cur"
        ;;
    files)
        _rip_root_flags "$cur"
        _rip_add -f -- "$cur"
        ;;
    --)
        _rip_add -f -- "$cur"
        ;;
    undo)
        _rip_add -W '-y --yes' -- "$cur"
        ;;
    list)
        _rip_add -W '-a --all -0 --null' -- "$cur"
        ;;
    restore)
        _rip_trashed "$cur"
        _rip_add -W '-a --all --rename -y --yes' -- "$cur"
        ;;
    empty)
        _rip_add -W '--older-than --max-size -y --yes' -- "$cur"
        ;;
    purge)
        _rip_trashed "$cur"
        _rip_add -W '-a --all -y --yes' -- "$cur"
        ;;
    esac
}

complete -F _rip rip
