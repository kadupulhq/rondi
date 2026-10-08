# DigitalOcean GitHub Actions runners

Rondi is public, so its workflows run on GitHub-hosted runners by default.
Eligible Linux jobs use the ephemeral DigitalOcean runners managed by
[github-runners-infra](https://github.com/somethingwithproof/github-runners-infra)
only when the repository Actions variable `RONDI_DO_RUNNERS_ENABLED` equals
`true`. Rondi's own workflows deliberately ignore the organization-wide
`DO_RUNNERS_ENABLED` variable. The reusable release workflow in
`kadupulhq/.github` still reads `DO_RUNNERS_ENABLED`; set it to `false` at
repository scope to keep tagged releases on GitHub-hosted runners.

Push, schedule, manual workflow events, and pull requests with branches in the
same repository are eligible. Fork pull requests and comment-triggered reviews
stay on GitHub-hosted runners. Explicit Windows and Ubuntu 22.04 compatibility jobs
keep their existing platforms. Reusable release jobs follow the calling
repository's event and variable.

Requested labels are `self-hosted`, `Linux`, `X64`, and `kadupul-do`. The last
label isolates this organization's jobs from other runner pools. These are
repository-scoped runners; changing the organization's default runner group
does not configure their provisioning.

Before enabling routing:

1. Install [ephemeral-runners-tv](https://github.com/apps/ephemeral-runners-tv/installations/new)
   on kadupulhq with access to all repositories, including future repositories.
2. Configure a dedicated Kadupul controller with that installation ID and all
   five repository identities: kadupul, template, terraform, .github, and rondi.
   The website repository is excluded from this rollout. Keep the existing somethingwithproof controller separate; its
   installation token cannot administer Kadupul repositories.
3. Verify public-repository authorization in the controller. The infrastructure
   project's current main branch explicitly rejects public repositories; the
   application installation and workflow labels alone do not change that.
   Retain signature verification, exact installation and repository checks,
   and controller-owned single-job provisioning and deletion. Runner VMs must
   not receive cloud credentials, controller callback credentials, or an App key.
4. Provide a compatible Linux x64 image with Docker, git, curl, jq, unzip, gh,
   a writable /opt/hostedtoolcache and noninteractive sudo for existing package
   installation steps. Verify action-specific dependencies during the pilot.
5. Manually run **DigitalOcean runner smoke** for this repository. This workflow
   deliberately bypasses the variable to test provisioning before CI cutover.
   Check successful registration, job completion, and actual droplet deletion.
   A job timeout does not limit the time spent waiting for a runner: cancel a
   queued smoke run if provisioning fails.
6. Set `RONDI_DO_RUNNERS_ENABLED=true` for this repository and verify an ordinary
   eligible workflow, including its runtime setup. Enable the remaining
   repositories only after provisioning and cleanup are confirmed.

Rollback: set `RONDI_DO_RUNNERS_ENABLED` to `false` and cancel queued self-hosted runs.
New eligible runs use GitHub-hosted runners; already queued jobs do not change
their runner selection. Re-run cancelled work after rollback. No credentials
belong in this document or repository variables.
