import { useEffect } from 'react'
import { useNavigate } from 'react-router'

interface KeyboardShortcutOptions {
  /**
   * The key to listen for (e.g., 'n', 'c', 'e')
   */
  key: string
  /**
   * The path to navigate to when the key is pressed
   */
  path?: string
  /**
   * Custom callback to execute instead of navigation
   */
  callback?: () => void
  /**
   * Whether the shortcut is enabled (default: true)
   */
  enabled?: boolean
}

/**
 * Selector for the Radix overlays that own the keyboard while they're open.
 * A bare letter shortcut must not fire underneath one of these — otherwise
 * pressing `N` inside an edit dialog navigates the page out from under it.
 */
const OPEN_OVERLAY_SELECTOR = [
  '[role="dialog"][data-state="open"]',
  '[role="alertdialog"][data-state="open"]',
  '[role="menu"][data-state="open"]',
  '[role="listbox"][data-state="open"]',
].join(',')

/**
 * Hook to register keyboard shortcuts that trigger navigation or callbacks.
 * Prevents triggering when the user is typing in an input field or when a
 * dialog, menu or select is open on top of the page.
 *
 * @example
 * // Navigate to create page on 'N' key
 * useKeyboardShortcut({ key: 'n', path: '/projects/new' })
 *
 * @example
 * // Execute custom callback on 'N' key
 * useKeyboardShortcut({ key: 'n', callback: () => setDialogOpen(true) })
 */
export function useKeyboardShortcut({
  key,
  path,
  callback,
  enabled = true,
}: KeyboardShortcutOptions) {
  const navigate = useNavigate()

  useEffect(() => {
    if (!enabled) return

    const handleKeyDown = (e: KeyboardEvent) => {
      // Check if user is typing in an input field
      const target = e.target as HTMLElement
      const isTyping =
        target.tagName === 'INPUT' ||
        target.tagName === 'TEXTAREA' ||
        target.isContentEditable

      // A dialog, menu or select on top of the page owns the keyboard.
      const overlayOpen = document.querySelector(OPEN_OVERLAY_SELECTOR) !== null

      // Only trigger if not typing and no modifier keys are pressed
      if (
        !isTyping &&
        !overlayOpen &&
        e.key.toLowerCase() === key.toLowerCase() &&
        !e.metaKey &&
        !e.ctrlKey &&
        !e.altKey &&
        !e.shiftKey
      ) {
        e.preventDefault()

        if (callback) {
          callback()
        } else if (path) {
          navigate(path)
        }
      }
    }

    document.addEventListener('keydown', handleKeyDown)
    return () => document.removeEventListener('keydown', handleKeyDown)
  }, [key, path, callback, enabled, navigate])
}
