# reeve

reeve (server) compiles desired state; reeve-agent (per box) converges on it.

A Margo-inspired fleet desired-state manager: a layered deployment tree
compiled into per-device git repos, converged by a pull-based agent.
See CLAUDE.md for the laws and layout.

## Setup
    git clone https://github.com/margo/specification spec   # pin PR2 tag
    # sandbox/reference implementation alongside:
    # git clone <margo sandbox repo> reference
    cargo build --workspace
