import { useEffect, useState, type ImgHTMLAttributes } from 'react';

/** An image from the gateway. A plain <img> cannot send the token a packaged app needs, so fetch it (the fetch carries
 *  the token) and show it from memory. */
export function ApiImage({ src, ...rest }: { src: string } & Omit<ImgHTMLAttributes<HTMLImageElement>, 'src'>) {
  const [shown, setShown] = useState<string | null>(null);
  useEffect(() => {
    let url: string | null = null;
    let cancelled = false;
    fetch(src)
      .then((r) => (r.ok ? r.blob() : Promise.reject(new Error(String(r.status)))))
      .then((b) => {
        if (cancelled) return;
        url = URL.createObjectURL(b);
        setShown(url);
      })
      .catch(() => setShown(null));
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [src]);
  return shown ? <img src={shown} {...rest} /> : null;
}
