# fdedup

fdedup scans the specified directory tree for files with different
names, but the same md5 hash. By default it prints a report, in json
format, of all found duplicate files and their names. Optionally it
can pass duplicates to another program via -exec, or delete all but
the shortest name via --keep-shortest.

```
fdedup 1.0.0

USAGE:
    fdedup [FLAGS] [OPTIONS] <path>

FLAGS:
    -h, --help               Prints help information
    -l, --ignore-symlinks    don't follow symlinks
        --keep-shortest      delete all but the shortest named duplicate
    -p, --pretend            only show what would be done
    -V, --version            Prints version information

OPTIONS:
        --exec <exec>                 pass each duplicate set to program
        --max-dirs <max-dirs>         max simultaneous open directories [default: 256]
        --max-files <max-files>       max simultaneous open files [default: 512]
        --max-symlinks <max-links>    max symlinks to traverse [default: 128]

ARGS:
    <path>
```

example

```
$ fdedup proj 2>/dev/null
...
{"digest":[47,188,21,116,50,152,178,14,75,64,19,93,209,168,218,138],"paths":["file0","file1"]}
...
```

demonstrating -exec

```
$ fdedup --exec ./print-dup.sh proj 2>/dev/null
```

print-dup.sh
```
#! /bin/bash

echo $@
```

output looks like e.g,

```
ee97dc2b732f200d616dae66216d57cc file0 file1
```

one duplicate file per line, starting with the hash.
