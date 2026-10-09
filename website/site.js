const search = document.querySelector('#doc-search');
if (search) {
  search.addEventListener('input', () => {
    const query = search.value.toLowerCase();
    document.querySelectorAll('.sidebar a').forEach(link => {
      link.hidden = !link.textContent.toLowerCase().includes(query);
    });
  });
}
