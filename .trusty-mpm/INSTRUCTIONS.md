## After every code change

After every fix or feature, **always** run the full release cycle:
1. `make version-patch` — bump patch version
2. Commit + push the version bump
3. `make publish` — publish all crates to crates.io

No exceptions. Every merged change ships a new patch release.

---
