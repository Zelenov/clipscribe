"""Prototype shot detection on 64x36 grey frames. Usage: shots.py luma.raw fps"""
import sys
W, H = 64, 36
data = open(sys.argv[1], "rb").read(); fps = float(sys.argv[2])
n = len(data) // (W * H)
F = [data[i*W*H:(i+1)*W*H] for i in range(n)]
def mad(a, b): return sum(abs(x - y) for x, y in zip(a, b)) * 100.0 / (len(a) * 255)
def mean(a): return sum(a) * 100.0 / (len(a) * 255)
def blend_err(a, m, b):  # how far m is from the mix of a and b that fits it best
    best = 99.0
    for k in range(1, 10):
        w = k / 10
        best = min(best, sum(abs(y - ((1-w)*x + w*z)) for x, y, z in zip(a, m, b)) * 100.0 / (len(a)*255))
    return best
d = [0.0] + [mad(F[i-1], F[i]) for i in range(1, n)]
sc = [0.0, 0.0] + [min(d[i], abs(d[i] - d[i-1])) for i in range(2, n)]  # ffmpeg scdet score
WIN = int(fps)
cuts, flashes, skip = [], [], set()
for i in range(2, n):
    if i in skip: continue
    # an isolated spike: well above every other score within 1 s (not counting i±1)
    nb = [sc[j] for j in range(max(2, i-WIN), min(n, i+WIN+1)) if abs(j - i) > 1 and j not in skip]
    if sc[i] < 0.5 or sc[i] < 4.0 * max(max(nb, default=0.0), 0.05):
        continue
    back = [(mad(F[i-1], F[j]), j) for j in range(i+1, min(n, i+5))]
    m = min(back) if back else (99, i)
    if m[0] < 0.35 * d[i]:          # the picture comes back: a flash, not a cut
        flashes.append(i); skip.update(range(i, m[1] + 1))
    else:
        cuts.append(i)
G = int(fps * 0.5)
grads, i = [], G
while i < n:
    a, b = i - G, i
    span, steps = mad(F[a], F[b]), max(d[a+1:b+1])
    if span >= 5.0 and steps < 0.35 * span and not any(a <= c <= b for c in cuts) \
       and blend_err(F[a], F[(a+b)//2], F[b]) < 0.25 * span:
        while b + 1 < n and mad(F[a], F[b+1]) > mad(F[a], F[b]) + 0.2 and d[b+1] < 0.35 * mad(F[a], F[b+1]):
            b += 1
        kind = "fade to black" if mean(F[b]) < 4 else ("fade from black" if mean(F[a]) < 4 else "dissolve")
        grads.append((a, b, kind)); i = b + G
    else:
        i += 1
t = lambda i: f"{i/fps:.2f}"
print("cuts:", [t(i) for i in cuts])
print("flashes ignored:", [t(i) for i in flashes])
print("gradual:", [(t(a), t(b), k) for a, b, k in grads])
bounds = sorted(set([0] + cuts + [a for a, _, _ in grads] + [n]))
print("shots:", len(bounds) - 1, [(t(a), t(b)) for a, b in zip(bounds, bounds[1:])])
