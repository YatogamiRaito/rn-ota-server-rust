# Security Policy

## Supported versions

The latest released `1.x` version receives security fixes. Older tags do not.

## Reporting a vulnerability

Please **do not** open a public issue for a security problem.

Use GitHub's private reporting — *Security* → *Report a vulnerability* on
[the repository](https://github.com/YatogamiRaito/rn-ota-server-rust/security/advisories/new) —
or email **ebubekirkaraca@aygyonetim.com**.

Include what you can: affected version, a description of the impact, and reproduction steps. You
will get an acknowledgement within a few days and a fix or an explanation before any public
disclosure.

## Deployment notes that matter for your own security

This server hands out presigned URLs to your bundles and accepts bundle metadata from the CLI, so
how you deploy it is part of its threat model:

- **The update-check endpoints are unauthenticated by design** — devices call them before they have
  any credential, exactly as with upstream hot-updater. Do not put anything secret in a bundle's
  metadata.
- **The CLI API is protected only by the per-app bearer token** (`AUTH_TOKEN_<SUFFIX>`). That token
  can create, modify and delete bundles for its app. Treat it like a deploy key: keep it out of the
  mobile app, out of the repository, and rotate it if it leaks.
- **Always terminate TLS in front of the server.** Bearer tokens travel in the `Authorization`
  header; over plain HTTP they are readable on the wire. Run it behind nginx/Caddy/a load balancer.
- **Do not expose the server on a public interface without a reverse proxy.** The default `HOST` is
  `127.0.0.1` for that reason; the Docker image sets `0.0.0.0` because the container boundary is
  the proxy boundary there.
- **Each app gets its own S3/R2 credentials.** Scope those keys to that app's bucket only — a
  compromised token then cannot reach another app's bundles.
- **The database user needs DDL rights** because migrations run at startup. If that is not
  acceptable in your environment, run the migrations out of band and give the runtime user
  DML-only rights.
