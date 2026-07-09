# harness-hat PHP image
#
# Build after harness-hat-base:local (from repo root):
#   docker build -t harness-hat-php:local -f docker/php.dockerfile docker/

FROM harness-hat-base:local

USER root

ENV COMPOSER_HOME=/home/coder/.composer
ENV PATH="${COMPOSER_HOME}/vendor/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      pkg-config \
      jq \
      shellcheck \
      direnv \
      sqlite3 \
      libsqlite3-dev \
      php-cli \
      php-dev \
      php-curl \
      php-mbstring \
      php-xml \
      php-zip \
      php-intl \
      php-bcmath \
      php-gd \
      php-mysql \
      php-pgsql \
      php-sqlite3 \
      php-soap \
      php-xdebug \
      php-pcov; \
    rm -rf /var/lib/apt/lists/*

# Verify the Composer installer against the hash Composer publishes at a fixed
# URL before executing it, per Composer's official install instructions. Without
# this, a compromised/MITM'd installer would run as part of the image build
# (H5). The `--check` flag makes the installer abort on a bad signature.
RUN set -eu; \
    curl -fsSL https://getcomposer.org/installer -o /tmp/composer-setup.php; \
    EXPECTED="$(curl -fsSL https://composer.github.io/installer.sig)"; \
    ACTUAL="$(php -r "echo hash_file('sha384', '/tmp/composer-setup.php');")"; \
    if [ "$EXPECTED" != "$ACTUAL" ]; then \
      echo "ERROR: composer installer signature mismatch" >&2; \
      rm -f /tmp/composer-setup.php; \
      exit 1; \
    fi; \
    php /tmp/composer-setup.php --install-dir=/usr/local/bin --filename=composer; \
    rm -f /tmp/composer-setup.php; \
    mkdir -p "${COMPOSER_HOME}"; \
    chown -R coder:coder "${COMPOSER_HOME}"

USER coder

ENV COMPOSER_HOME=/home/coder/.composer
ENV PATH="${COMPOSER_HOME}/vendor/bin:/home/coder/.local/bin:${PATH}"

# Pinned tool versions (H5). Composer verifies package dist archives against
# the hashes in Packagist metadata, so an exact version is also a content pin.
# phpunit stays on the 12.x line: 13.x requires PHP >= 8.4.1 and Ubuntu 24.04
# ships PHP 8.3. Bump by editing the versions and rebuilding.
RUN set -eu; \
    composer global require \
      phpunit/phpunit:12.5.31 \
      friendsofphp/php-cs-fixer:v3.95.12 \
      phpstan/phpstan:2.2.5 \
      laravel/pint:v1.29.3

CMD ["bash"]
