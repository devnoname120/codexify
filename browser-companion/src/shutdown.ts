/** A failed driver must not keep the companion alive after Chrome has closed. */
export async function shutdownWithDeadline(
  close: () => Promise<void>, forceExit: () => void, timeoutMs = 10_000,
): Promise<void> {
  let timer: ReturnType<typeof setTimeout>;
  const deadline = new Promise<void>(resolve => {
    timer = setTimeout(() => { forceExit(); resolve(); }, timeoutMs);
  });
  try { await Promise.race([Promise.resolve().then(close), deadline]); }
  finally { clearTimeout(timer!); }
}
