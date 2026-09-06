# Recipes

[Quick start](../README.md#quick-start) · [Recipe catalogue](https://github.com/kierandrewett/pbox-recipes)

Recipes are Ansible playbooks that install and configure software inside a box.

## Install and discover

Install [Ansible](https://docs.ansible.com/projects/ansible/latest/installation_guide/intro_installation.html)
and Git on the machine running pbox:

```sh
ansible-playbook --version
pbox recipe list
pbox recipe search browser
pbox recipe info language/rust
```

Pbox downloads the configured recipe repository. You do not need to clone it
manually or create an Ansible inventory.

## Apply recipes

```sh
pbox recipe apply --box-id current dev/base language/rust
```

Several recipes share preparation and run in the order given. They execute as
root through the guest agent. Distribution and CPU support vary by recipe.

Pbox prepares Python for supported package managers. If a minimal guest needs
manual preparation, install Python inside it first, then retry the recipe.
Graphical apps also need a [desktop session](desktop.md).

## Progress and logs

Normal output shows preparation, tasks, results and available Ansible logs.
Some modules return their output only after finishing.

```sh
pbox --verbose recipe apply --box-id current dev/base
```

`--verbose` streams raw Ansible output. Failed runs show a full log path;
successful runs remove their temporary logs. For structured results, use
`--json`.

## Rollback and storage

Pbox can create a temporary Proxmox checkpoint before applying recipes.

| `recipes.snapshot-before-apply` | Behaviour |
| --- | --- |
| `auto` | Default: take a checkpoint if supported; warn and continue if unavailable |
| `always` | Require a checkpoint; fail before applying if it cannot be created |
| `never` | Apply without a checkpoint |

```sh
pbox config set recipes.snapshot-before-apply always
pbox config set recipes.rollback-on-failure true
```

Automatic rollback needs a successfully created checkpoint. Without one, a
failed recipe can leave partial changes. Check the error and log before retrying.

These temporary checkpoints differ from independent
[pbox snapshots](snapshots.md), which create reusable environments.

## Choose a repository or revision

```sh
pbox config set recipes.repository https://github.com/kierandrewett/pbox-recipes.git
pbox config set recipes.ref main
pbox recipe sync
```

Use a branch, tag or commit supported by the repository when selecting a
revision. Recipe code runs with root access inside the box, so select a source
you trust. Repository settings are listed by `pbox config list`.
