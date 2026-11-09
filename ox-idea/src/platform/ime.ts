/**
 * True while an input method is composing, including the key that ends it: WebKit sends the
 * Enter / Space that commits a candidate after `compositionend`, with `isComposing` already
 * false and keyCode 229, so a handler checking `isComposing` alone runs that Enter.
 */
export function isImeKey(e: KeyboardEvent): boolean {
  return e.isComposing || e.keyCode === 229;
}
