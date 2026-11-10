.PHONY: update restart

# Take the new image from CI, start it, follow the logs.
update:
	docker compose pull
	docker compose up -d
	docker compose logs -f

# Restart the container (it reads data/config.toml again), follow the logs.
restart:
	docker compose restart tg-digest
	docker compose logs -f
