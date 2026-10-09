const target = document.querySelector('#redirect-target');
if (target) {
  const destination = new URL(target.href);
  destination.hash = window.location.hash;
  window.location.replace(destination.href);
}
