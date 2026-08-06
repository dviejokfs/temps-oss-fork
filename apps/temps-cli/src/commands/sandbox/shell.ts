/**
 * `temps sandbox shell` — an interactive terminal in a sandbox, in your own
 * terminal.
 *
 * Connects to `GET /v1/sandboxes/{id}/terminal`, which bridges to the
 * in-sandbox PTY agent (ADR-008). The agent keeps the tab alive with zero
 * subscribers, so this is a *reattachable* session: disconnecting — Ctrl-D,
 * a dropped network, closing your laptop — leaves whatever is running in
 * there running. Reconnect to the same `--tab` and you land back in it,
 * with recent scrollback replayed.
 *
 * That is what makes `temps sandbox shell ID --cmd claude` practical: the
 * agent survives your connection, so a long-running AI session isn't tied
 * to your laptop staying awake.
 *
 * Caveat worth knowing: suspension is not disconnection. The PTY lives in
 * the container's tmpfs, so if the sandbox is stopped (idle sweep, restart)
 * every tab is gone. While you're attached the server heartbeats activity
 * to keep the idle sweep away, so this only bites if you disconnect and
 * leave it idle past the timeout.
 */
import WebSocket from 'ws'

import { requireAuth, credentials, config } from '../../config/store.js'
import { normalizeApiUrl } from '../../lib/api-client.js'
import { colors, info, newline, warning } from '../../ui/output.js'
import { sandboxUrl } from './index.js'

export interface ShellOptions {
  tab?: string
  cmd?: string
}

/**
 * Turn the HTTP API base into the WebSocket URL for a sandbox terminal.
 *
 * `https` → `wss` (and `http` → `ws`); anything else is left alone so a
 * caller who already passed a `ws://` URL isn't second-guessed.
 */
export function terminalWsUrl(
  baseUrl: string,
  id: string,
  params: Record<string, string | undefined>,
): string {
  const http = sandboxUrl(baseUrl, `/${encodeURIComponent(id)}/terminal`)
  const ws = http.replace(/^http:/, 'ws:').replace(/^https:/, 'wss:')
  const query = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== '')
    .map(([k, v]) => `${k}=${encodeURIComponent(v as string)}`)
    .join('&')
  return query ? `${ws}?${query}` : ws
}

/** Current terminal size, falling back to a sane default when not a TTY. */
function terminalSize(): { cols: number; rows: number } {
  return {
    cols: process.stdout.columns || 80,
    rows: process.stdout.rows || 24,
  }
}

export async function shellAction(
  id: string,
  options: ShellOptions,
): Promise<void> {
  await requireAuth()
  const apiKey = await credentials.getApiKey()
  if (!apiKey) {
    throw new Error('Not authenticated. Run `temps login` first.')
  }
  if (!process.stdin.isTTY) {
    throw new Error(
      'sandbox shell needs an interactive terminal. ' +
        'For scripted commands use: temps sandbox exec ' +
        id +
        ' -- <command>',
    )
  }

  const baseUrl = normalizeApiUrl(config.get('apiUrl'))
  const { cols, rows } = terminalSize()
  const url = terminalWsUrl(baseUrl, id, {
    tab: options.tab,
    cmd: options.cmd,
    cols: String(cols),
    rows: String(rows),
  })

  // The token goes in a header, never the query string: URLs end up in
  // proxy logs and shell history, and this one is a live credential.
  const socket = new WebSocket(url, {
    headers: { Authorization: `Bearer ${apiKey}` },
  })

  await runSession(socket, id, options)
}

function runSession(
  socket: WebSocket,
  id: string,
  options: ShellOptions,
): Promise<void> {
  return new Promise((resolve, reject) => {
    let rawModeEngaged = false
    let exitCode = 0
    let settled = false

    // Every exit path goes through here. Leaving the terminal in raw mode
    // is the classic failure of this kind of tool — the user's shell is
    // then silently broken and needs `reset`.
    const restore = () => {
      if (rawModeEngaged && process.stdin.isTTY) {
        process.stdin.setRawMode(false)
        rawModeEngaged = false
      }
      process.stdin.pause()
      process.stdin.removeListener('data', onStdin)
      process.stdout.removeListener('resize', onResize)
    }

    const finish = (err?: Error) => {
      if (settled) return
      settled = true
      restore()
      if (err) reject(err)
      else resolve()
    }

    const onStdin = (chunk: Buffer) => {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(chunk)
      }
    }

    const onResize = () => {
      if (socket.readyState !== WebSocket.OPEN) return
      const { cols, rows } = terminalSize()
      socket.send(JSON.stringify({ type: 'resize', cols, rows }))
    }

    socket.on('open', () => {
      process.stdin.setRawMode(true)
      rawModeEngaged = true
      process.stdin.resume()
      process.stdin.on('data', onStdin)
      process.stdout.on('resize', onResize)
    })

    socket.on('message', (data: Buffer, isBinary: boolean) => {
      if (isBinary) {
        process.stdout.write(data)
        return
      }
      // Text frames are control events, not terminal output — writing them
      // to stdout would splatter JSON into the user's session.
      let event: { type?: string; code?: number; message?: string }
      try {
        event = JSON.parse(data.toString())
      } catch {
        return
      }
      if (event.type === 'exit') {
        exitCode = event.code ?? 0
        socket.close()
      } else if (event.type === 'error') {
        restore()
        newline()
        warning(`Terminal error: ${event.message ?? 'unknown'}`)
      }
      // 'ready' needs no output: the shell's own prompt is the signal, and
      // announcing it would corrupt a reattached screen's replayed state.
    })

    socket.on('error', (err: Error) => {
      finish(
        new Error(
          `Could not open a terminal in ${id}: ${err.message}. ` +
            'Check the sandbox exists and is reachable with: temps sandbox show ' +
            id,
        ),
      )
    })

    socket.on('close', (code: number, reason: Buffer) => {
      restore()
      // 1000/1005/1006 are ordinary closes (clean, no-status, and the
      // "abnormal" code browsers use for a dropped TCP connection). Only a
      // genuine protocol-level failure is worth reporting as an error.
      if (code !== 1000 && code !== 1005 && code !== 1006) {
        const detail = reason.toString().trim()
        finish(
          new Error(
            `Terminal closed unexpectedly (code ${code})${detail ? `: ${detail}` : ''}`,
          ),
        )
        return
      }
      newline()
      info(
        `Detached from ${colors.primary(id)} (tab ${options.tab ?? 'main'}). ` +
          'Anything running in it keeps running — reattach with the same command.',
      )
      if (exitCode !== 0) {
        process.exitCode = exitCode
      }
      finish()
    })
  })
}
