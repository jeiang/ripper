# Completions for rip. Written by hand: clap's fish generator emits nothing for
# positional arguments, and rip mixes files with subcommands at the first word.

# Prints where the cursor is: "first" (no word yet), "files" (after a file),
# "--" (after --), or the subcommand name.
function __rip_state
    set -l skip
    for t in (commandline -xpc)[2..]
        if set -q skip[1]
            set -e skip
            continue
        end
        switch $t
            case --
                echo -- --
                return
            case --config --completions
                set skip 1
            case '-?*'
            case '*'
                if contains -- $t undo list restore empty purge
                    echo $t
                else
                    echo files
                end
                return
        end
    end
    echo first
end

function __rip_is
    contains -- (__rip_state) $argv
end

# Trashed original paths under the current directory, with the deletion date
# as the description.
function __rip_trashed
    rip list -0 2>/dev/null | string split0 | string replace -r '^([^\t]*)\t(.*)$' '$2\t$1'
end

complete -c rip -n '__rip_is first' -a undo -d 'Restore the last batch'
complete -c rip -n '__rip_is first' -a list -d 'List trashed items under this directory'
complete -c rip -n '__rip_is first' -a restore -d 'Restore trashed items'
complete -c rip -n '__rip_is first' -a empty -d 'Permanently delete trashed items'
complete -c rip -n '__rip_is first' -a purge -d 'Permanently delete chosen items'

complete -c rip -n '__rip_is first files' -s f -l force -d 'Ignore missing files, never prompt'
complete -c rip -n '__rip_is first files' -s i -d 'Prompt before every item'
complete -c rip -n '__rip_is first files' -s I -d 'Prompt once before more than 3 items or a directory'
complete -c rip -n '__rip_is first files' -s v -l verbose -d 'Print each item'
complete -c rip -n '__rip_is first files' -s r -s R -l recursive -d 'Ignored: trash is always recursive'
complete -c rip -n '__rip_is first files' -s d -l dir -d 'Ignored'
complete -c rip -n 'not __rip_is --' -l config -r -F -d 'Config file'
complete -c rip -n '__rip_is first' -s h -l help -d 'Print help'
complete -c rip -n '__rip_is first' -s V -l version -d 'Print version'

complete -c rip -n '__rip_is undo list restore empty purge' -f
complete -c rip -n '__rip_is restore purge' -a '(__rip_trashed)'
complete -c rip -n '__rip_is list restore purge' -s a -l all -d 'All items, not only under this directory'
complete -c rip -n '__rip_is list' -s 0 -l null -d 'End records with NUL'
complete -c rip -n '__rip_is restore' -l rename -d 'Restore beside an existing path as name~N'
complete -c rip -n '__rip_is undo restore empty purge' -s y -l yes -d 'Do not prompt'
complete -c rip -n '__rip_is empty' -l older-than -x -d 'Delete items older than DUR (30d, 2w, 12h)'
complete -c rip -n '__rip_is empty' -l max-size -x -d 'Keep the newest items up to SIZE (500M, 50G)'
