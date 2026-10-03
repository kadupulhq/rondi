# DigitalOcean GitHub Actions runners

Eligible Linux jobs use the ephemeral DigitalOcean runners managed by
[github-runners-infra](https://github.com/somethingwithproof/github-runners-infra)
when the Actions variable `DO_RUNNERS_ENABLED` equals `true`. An unset or false
variable keeps the existing GitHub-hosted runner selection. The variable can be
set for a single repository during rollout, then at organization scope with
access to all repositories.

Only push, schedule, and manual workflow events are eligible. Pull requests
(including same-repository pull requests) and comment-triggered reviews stay
on GitHub-hosted runners. Explicit Windows and Ubuntu 22.04 compatibility jobs
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
   six repository identities: kadupul, template, website, terraform, .github,
   and rondi. Keep the existing somethingwithproof controller separate; its
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
6. Set `DO_RUNNERS_ENABLED=true` for a pilot repository and verify an ordinary
   eligible workflow, including its runtime setup. Enable the remaining
   repositories only after provisioning and cleanup are confirmed.

Rollback: set the variable to `false` and cancel queued self-hosted runs.
New eligible runs use GitHub-hosted runners; already queued jobs do not change
their runner selection. Re-run cancelled work after rollback. No credentials
belong in this document or repository variables.
