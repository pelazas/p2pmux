# p2pmux.com

A Cloudflare Pages project. Three pages and one shell script, no build step.

| Path | What it is |
| --- | --- |
| `/` | Landing page. Deliberately minimal: the install command is the point |
| `/trust` | What a join code actually grants, and what `rv.p2pmux.com` can and cannot see |
| `/install.sh` | The installer, served `text/plain` so it can be read in a browser |

`install.sh` fetches binaries and their SHA256 from **GitHub Releases**, never from this domain.
That is deliberate: whoever controls the domain can break an install, but cannot substitute a
binary for the one published and hashed on GitHub.

## Deploying

```sh
export CLOUDFLARE_API_TOKEN=...   # Cloudflare Pages: Edit, plus Zone DNS: Edit for the domain
export CLOUDFLARE_ACCOUNT_ID=...
npx wrangler@4 pages deploy public --project-name p2pmux-site
```

## Not here on purpose

`/room` — the launch lead magnet. Delivery is still undecided (self-serve page versus manual DM),
and the launch plan's own metric depends on two qualification questions that a self-serve page
would bypass. Decide the mechanism before building the page.
