# empath

Track path accesses, query for frequently or recently accessed paths.

```
$ for i in $(seq 1 10); do empath record src/main.rs; done

$ empath record README.md

$ empath query frequent
src/main.rs
README.md

$ empath query recent
README.md
src/main.rs

$ vim $(empath query frecent | fzf)
```
