find . -type f -name '*.log' -mtime -1 \
  -exec grep -l 'ERROR\|panic' {} + \
  | xargs -I{} sh -c 'echo "==> {}"; tail -n 20 "{}"'

# Quick disk-hog check
du -h --max-depth=2 /var/log 2>/dev/null | sort -hr | head -20
