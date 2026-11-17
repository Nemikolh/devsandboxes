import { describe, expect, test } from 'bun:test'
import { createTestHost } from 'zidane/extensions/testing'
import { uncoveredCargo } from './detect'
import extension from './index'

const EXEMPT = new Set(['--version', '-V', 'fmt', 'metadata'])
const flagged = (cmd: string) => uncoveredCargo(cmd, EXEMPT).map(u => u.subcommand)

describe('uncoveredCargo', () => {
  test('flags bare cargo calls', () => {
    expect(flagged('cargo test --workspace')).toEqual(['test'])
    expect(flagged('cd src && cargo build')).toEqual(['build'])
    expect(flagged('RUST_LOG=debug cargo +nightly test -p x')).toEqual(['test'])
    expect(flagged('cargo -q test')).toEqual(['test'])
    expect(flagged('~/.cargo/bin/cargo nextest run')).toEqual(['nextest'])
    expect(flagged('cargo test 2>&1 | tail -50')).toEqual(['test'])
    expect(flagged('timeout 60 cargo build; cargo test')).toEqual(['test'])
    expect(flagged('cargo test | timeout 5 tee log')).toEqual(['test'])
    expect(flagged('echo "$(cargo test)"')).toEqual([])  // inside quotes: not followed
    expect(flagged('x=$(cargo test)')).toEqual(['test'])
    expect(flagged("bash -c 'cargo test'")).toEqual(['test'])
    expect(flagged('env FOO=1 nice -n 5 cargo run')).toEqual(['run'])
    expect(flagged('if true; then cargo test; fi')).toEqual(['test'])
  })

  test('accepts timeout-wrapped calls', () => {
    expect(flagged('timeout 600 cargo test --workspace')).toEqual([])
    expect(flagged('timeout -k 10 600 cargo test')).toEqual([])
    expect(flagged('timeout --signal=KILL 5m cargo test')).toEqual([])
    expect(flagged('timeout -s INT 5m cargo test')).toEqual([])
    expect(flagged('FOO=1 timeout 60 env BAR=2 cargo test')).toEqual([])
    expect(flagged("timeout 60 bash -c 'cargo test && cargo build'")).toEqual([])
    expect(flagged('cd x && timeout 60 cargo test 2>&1 | tail')).toEqual([])
  })

  test('ignores exempt subcommands and non-cargo mentions', () => {
    expect(flagged('cargo --version')).toEqual([])
    expect(flagged('cargo fmt')).toEqual([])
    expect(flagged('grep cargo Cargo.toml')).toEqual([])
    expect(flagged('echo cargo test')).toEqual([])
    expect(flagged('ls target # cargo test')).toEqual([])
    expect(flagged('cat ~/.cargo/config.toml')).toEqual([])
  })
})

describe('extension', () => {
  const call = (command: unknown) => ({
    tool: 'shell', displayName: 'shell', input: { command }, callId: 'c', turnId: 't', runToolCounts: {},
  })

  test('blocks unwrapped cargo, allows the rest', async () => {
    const host = await createTestHost({ extensions: [extension], settings: { 'cargo-timeouts': { seconds: 300 } } })
    expect(host.extensions()[0]?.status).toBe('active')

    const blocked = await host.intercept('tool.call', call('cargo test --workspace'), 'allow', { matchKey: 'shell' })
    expect(blocked).toMatchObject({ block: expect.stringContaining('timeout -k 10 300 cargo test --workspace') })

    const redirected = await host.intercept('tool.call', call('cargo test --workspace 2>&1 >&2'), 'allow', { matchKey: 'shell' })
    expect(redirected).toMatchObject({ block: expect.stringContaining('`timeout -k 10 300 cargo test --workspace 2>&1 >&2`') })

    expect(await host.intercept('tool.call', call('timeout 300 cargo test'), 'allow', { matchKey: 'shell' })).toBe('allow')
    expect(await host.intercept('tool.call', call('ls'), 'allow', { matchKey: 'shell' })).toBe('allow')
    expect(await host.intercept('tool.call', call(undefined), 'allow', { matchKey: 'shell' })).toBe('allow')
    await host.dispose()
  })
})
