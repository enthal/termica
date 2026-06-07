# media-src — raw homepage captures

Drop full-resolution (2× / retina) PNG screenshots here, then run the
optimizer to generate the web variants the page actually serves:

```sh
python3 infra/optimize-media.py   # reads this dir → writes src/img/
./infra/deploy.sh                 # publish
```

Expected files (drop what you have; missing ones are skipped):

| File | Used for |
| --- | --- |
| `hero.png` | the 3-pane workspace hero (also becomes the 1200×630 `og.png`) |
| `history.png` | the Ctrl+R history overlay feature row |
| `completion.png` | the Tab-completion popup feature row |

These originals are the source of truth; everything in `src/img/` is generated
from them and can be regenerated at any time.
