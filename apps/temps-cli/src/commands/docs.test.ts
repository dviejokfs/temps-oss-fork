import { afterEach, describe, expect, test } from 'bun:test'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { generateDocs } from './docs.js'

const tempDirs: string[] = []

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((path) => rm(path, { recursive: true, force: true })))
})

describe('generateDocs', () => {
  test('writes the complete command catalog without requiring Bun globals', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'temps-cli-docs-'))
    tempDirs.push(directory)
    const output = join(directory, 'commands.json')

    await generateDocs({ format: 'json', output })

    const commands = JSON.parse(await readFile(output, 'utf8')) as Array<{ name: string }>
    expect(commands.some((command) => command.name === 'otel-forward')).toBe(true)
    expect(commands.some((command) => command.name === 'projects')).toBe(true)
    expect(commands.some((command) => command.name === 'cloud')).toBe(true)
  })

  test('keeps the skill command appendix synchronized with the CLI', async () => {
    const directory = await mkdtemp(join(tmpdir(), 'temps-cli-docs-'))
    tempDirs.push(directory)
    const output = join(directory, 'COMMANDS.md')
    const repositoryRoot = join(dirname(fileURLToPath(import.meta.url)), '../../../..')
    const committedReference = join(
      repositoryRoot,
      'skills/temps-cli/references/COMMANDS.md',
    )

    await generateDocs({ format: 'markdown', output })

    expect(await readFile(output, 'utf8')).toBe(await readFile(committedReference, 'utf8'))
  })
})
