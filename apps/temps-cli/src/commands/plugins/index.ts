import type { Command } from 'commander'
import { listPluginsAction } from './list.js'
import { installPluginAction } from './install.js'

export function registerPluginsCommands(program: Command): void {
  const plugins = program
    .command('plugins')
    .description('Discover and install external Temps plugins (e.g. VibeTemps)')

  plugins
    .command('list')
    .alias('ls')
    .description('List plugins available for install and whether they are already installed')
    .option('--json', 'Output in JSON format')
    .action(listPluginsAction)

  plugins
    .command('install <name>')
    .description('Download, verify, and install an external plugin binary')
    .option('--version <version>', 'Specific version hint (currently unused server-side; install always fetches latest)')
    .option('--json', 'Output in JSON format')
    .action(installPluginAction)
}
