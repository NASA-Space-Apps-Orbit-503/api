-- Esquema de la base. Se ejecuta al arrancar la API; es idempotente.

-- ---------- Instrument ----------
CREATE SEQUENCE IF NOT EXISTS instruments_id_seq START 1;
CREATE TABLE IF NOT EXISTS instruments (
    id   BIGINT  PRIMARY KEY DEFAULT nextval('instruments_id_seq'),
    code VARCHAR NOT NULL UNIQUE,  -- MODIS, VIIRS, ...
    name VARCHAR NOT NULL
);

INSERT INTO instruments (code, name) VALUES
    ('MODIS', 'Moderate Resolution Imaging Spectroradiometer'),
    ('VIIRS', 'Visible Infrared Imaging Radiometer Suite')
ON CONFLICT (code) DO NOTHING;

-- ---------- Detection (hotspot FIRMS) ----------
CREATE SEQUENCE IF NOT EXISTS detections_id_seq START 1;
CREATE TABLE IF NOT EXISTS detections (
    id           BIGINT  PRIMARY KEY DEFAULT nextval('detections_id_seq'),
    latitude     DOUBLE  NOT NULL,
    longitude    DOUBLE  NOT NULL,
    bright_ti4   DOUBLE,
    scan         DOUBLE,
    track        DOUBLE,
    acq_date     DATE    NOT NULL,
    acq_time     VARCHAR NOT NULL,  -- HHMM en UTC, igual que FIRMS
    satellite    VARCHAR NOT NULL,
    instrument   VARCHAR NOT NULL REFERENCES instruments (code),
    confidence   VARCHAR,
    "version"    VARCHAR,
    bright_ti5   DOUBLE,
    frp          DOUBLE,
    daynight     VARCHAR            -- D | N
);
CREATE INDEX IF NOT EXISTS detections_acq_date_idx ON detections (acq_date);

-- La misma detección (lugar, fecha, hora, satélite e instrumento) no se guarda dos veces.
-- Así, importar el mismo CSV de FIRMS dos veces no duplica datos.
CREATE UNIQUE INDEX IF NOT EXISTS detections_natural_key
    ON detections (latitude, longitude, acq_date, acq_time, satellite, instrument);

-- Vista armonizada: agrega la confianza normalizada a una escala común.
--   VIIRS: l / n / h          → 1 / 2 / 3
--   MODIS: 0-29 / 30-79 / 80+ → 1 / 2 / 3   (umbrales de la guía de usuario de FIRMS)
CREATE OR REPLACE VIEW detections_harmonized AS
SELECT
    *,
    CASE
        WHEN lower(confidence) IN ('l', 'low') THEN 1
        WHEN lower(confidence) IN ('n', 'nominal') THEN 2
        WHEN lower(confidence) IN ('h', 'high') THEN 3
        WHEN TRY_CAST(confidence AS DOUBLE) IS NULL THEN NULL
        WHEN TRY_CAST(confidence AS DOUBLE) < 30 THEN 1
        WHEN TRY_CAST(confidence AS DOUBLE) < 80 THEN 2
        ELSE 3
    END AS confidence_level
FROM detections;

-- ---------- Burning Activity Stats ----------
CREATE SEQUENCE IF NOT EXISTS burning_activity_stats_id_seq START 1;
CREATE TABLE IF NOT EXISTS burning_activity_stats (
    id       BIGINT  PRIMARY KEY DEFAULT nextval('burning_activity_stats_id_seq'),
    "day"    INTEGER NOT NULL,
    "month"  INTEGER NOT NULL,
    "year"   INTEGER NOT NULL,
    "count"  BIGINT  NOT NULL,
    max_frp  DOUBLE
);
