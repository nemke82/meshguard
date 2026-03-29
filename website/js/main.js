// ─── Matrix rain background ───────────────────────────────
const canvas = document.getElementById('matrix-rain');
if (canvas) {
  const ctx = canvas.getContext('2d');
  let w, h, columns, drops;
  const chars = 'MESHGUARD01アイウエオカキクケコ>_|/\\+=.';
  const fontSize = 14;

  function initMatrix() {
    w = canvas.width = canvas.offsetWidth;
    h = canvas.height = canvas.offsetHeight;
    columns = Math.floor(w / fontSize);
    drops = Array.from({ length: columns }, () => Math.random() * -50);
  }

  function drawMatrix() {
    ctx.fillStyle = 'rgba(10, 15, 13, 0.06)';
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = '#00ff88';
    ctx.font = `${fontSize}px JetBrains Mono, monospace`;

    for (let i = 0; i < columns; i++) {
      const char = chars[Math.floor(Math.random() * chars.length)];
      const x = i * fontSize;
      const y = drops[i] * fontSize;

      ctx.globalAlpha = 0.4 + Math.random() * 0.4;
      ctx.fillText(char, x, y);

      if (y > h && Math.random() > 0.975) {
        drops[i] = 0;
      }
      drops[i] += 0.5 + Math.random() * 0.3;
    }
    ctx.globalAlpha = 1;
    requestAnimationFrame(drawMatrix);
  }

  initMatrix();
  drawMatrix();
  window.addEventListener('resize', initMatrix);
}

// ─── Scroll-reveal observer ───────────────────────────────
const revealObserver = new IntersectionObserver(
  (entries) => {
    entries.forEach((entry) => {
      if (entry.isIntersecting) {
        entry.target.classList.add('visible');
        revealObserver.unobserve(entry.target);
      }
    });
  },
  { threshold: 0.12, rootMargin: '0px 0px -40px 0px' }
);

document.querySelectorAll('.reveal').forEach((el) => {
  revealObserver.observe(el);
});

// Stagger animations
document.querySelectorAll('.feature-card.reveal').forEach((card, i) => {
  card.style.transitionDelay = `${i * 70}ms`;
});

document.querySelectorAll('.download-card.reveal').forEach((card, i) => {
  card.style.transitionDelay = `${i * 90}ms`;
});

// ─── Navbar scroll effect ─────────────────────────────────
const nav = document.getElementById('nav');

window.addEventListener('scroll', () => {
  nav.classList.toggle('scrolled', window.scrollY > 40);
}, { passive: true });

// ─── Mobile nav toggle ───────────────────────────────────
const navToggle = document.getElementById('nav-toggle');
const navLinks = document.querySelector('.nav-links');

navToggle.addEventListener('click', () => {
  navToggle.classList.toggle('open');
  navLinks.classList.toggle('open');
});

navLinks.querySelectorAll('a').forEach((link) => {
  link.addEventListener('click', () => {
    navToggle.classList.remove('open');
    navLinks.classList.remove('open');
  });
});

// ─── Smooth scroll for anchor links ──────────────────────
document.querySelectorAll('a[href^="#"]').forEach((anchor) => {
  anchor.addEventListener('click', (e) => {
    const target = document.querySelector(anchor.getAttribute('href'));
    if (target) {
      e.preventDefault();
      const offset = nav.offsetHeight + 16;
      const top = target.getBoundingClientRect().top + window.scrollY - offset;
      window.scrollTo({ top, behavior: 'smooth' });
    }
  });
});
