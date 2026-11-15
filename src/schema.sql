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
    confidence   VARCHAR,           -- low | nominal | high
    "version"    VARCHAR,
    bright_ti5   DOUBLE,
    frp          DOUBLE,
    daynight     VARCHAR,           -- D | N
    "type"       INTEGER            -- 0 vegetación, 1 volcán, 2 otra fuente estática, 3 mar (archivo SP)
);
-- Bases creadas antes de existir la columna "type".
ALTER TABLE detections ADD COLUMN IF NOT EXISTS "type" INTEGER;
CREATE INDEX IF NOT EXISTS detections_acq_date_idx ON detections (acq_date);

-- La misma detección (lugar, fecha, hora, satélite e instrumento) no se guarda dos veces.
-- Así, importar el mismo CSV de FIRMS dos veces no duplica datos.
CREATE UNIQUE INDEX IF NOT EXISTS detections_natural_key
    ON detections (latitude, longitude, acq_date, acq_time, satellite, instrument);

-- La confianza se guarda como categoría (la API convierte al validar). Bases viejas o cargas
-- con el CLI de DuckDB: VIIRS l / n / h y MODIS 0-29 / 30-79 / 80-100 (umbrales de la guía
-- de FIRMS) pasan a low / nominal / high; un valor no reconocido queda en NULL.
UPDATE detections SET confidence = CASE
        WHEN lower(trim(confidence)) IN ('l', 'low') THEN 'low'
        WHEN lower(trim(confidence)) IN ('n', 'nominal') THEN 'nominal'
        WHEN lower(trim(confidence)) IN ('h', 'high') THEN 'high'
        WHEN TRY_CAST(confidence AS DOUBLE) BETWEEN 0 AND 100 THEN CASE
            WHEN TRY_CAST(confidence AS DOUBLE) < 30 THEN 'low'
            WHEN TRY_CAST(confidence AS DOUBLE) < 80 THEN 'nominal'
            ELSE 'high' END
    END
WHERE confidence IS NOT NULL AND confidence NOT IN ('low', 'nominal', 'high');

-- Vista armonizada: agrega la confianza como número (1 low, 2 nominal, 3 high) para el
-- filtro min_confidence y el nombre unificado del satélite (FIRMS usa distintos códigos
-- según el archivo).
--   satellite_name: Terra, Aqua, Suomi NPP, NOAA-20, NOAA-21 o NULL si el código no se reconoce.
CREATE OR REPLACE VIEW detections_harmonized AS
SELECT
    *,
    CASE confidence
        WHEN 'low' THEN 1
        WHEN 'nominal' THEN 2
        WHEN 'high' THEN 3
    END AS confidence_level,
    CASE regexp_replace(lower(trim(satellite)), '\.0$', '')
        WHEN 'terra' THEN 'Terra'
        WHEN 't' THEN 'Terra'
        WHEN 'aqua' THEN 'Aqua'
        WHEN 'a' THEN 'Aqua'
        WHEN 'n' THEN 'Suomi NPP'
        WHEN 'npp' THEN 'Suomi NPP'
        WHEN 'snpp' THEN 'Suomi NPP'
        WHEN 'suomi npp' THEN 'Suomi NPP'
        WHEN 'suomi-npp' THEN 'Suomi NPP'
        WHEN 'n20' THEN 'NOAA-20'
        WHEN '1' THEN 'NOAA-20'
        WHEN 'j1' THEN 'NOAA-20'
        WHEN 'noaa-20' THEN 'NOAA-20'
        WHEN 'noaa20' THEN 'NOAA-20'
        WHEN 'n21' THEN 'NOAA-21'
        WHEN '2' THEN 'NOAA-21'
        WHEN 'j2' THEN 'NOAA-21'
        WHEN 'noaa-21' THEN 'NOAA-21'
        WHEN 'noaa21' THEN 'NOAA-21'
    END AS satellite_name
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

-- ---------- Armonización VIIRS → MODIS (serie mensual) ----------
-- Tablas derivadas: se borran y se recalculan enteras en cada importación
-- (o con POST /stats/harmonization/rebuild). El método está en src/harmonization.rs.
CREATE SEQUENCE IF NOT EXISTS harmonization_runs_id_seq START 1;
CREATE TABLE IF NOT EXISTS harmonization_runs (
    id                           BIGINT    PRIMARY KEY DEFAULT nextval('harmonization_runs_id_seq'),
    computed_at                  TIMESTAMP NOT NULL,
    status                       VARCHAR   NOT NULL,  -- ok | no_overlap | insufficient_data
    detections                   BIGINT    NOT NULL,  -- tras quitar duplicados y aplicar el área
    after_confidence             BIGINT    NOT NULL,
    after_type                   BIGINT    NOT NULL,
    static_cells                 BIGINT    NOT NULL,
    after_static                 BIGINT    NOT NULL,
    used                         BIGINT    NOT NULL,  -- Aqua + el satélite VIIRS de referencia
    first_month                  DATE,
    last_month                   DATE,
    overlap_start                DATE,
    overlap_end                  DATE,
    overlap_months               BIGINT    NOT NULL,
    mean_raw_diff_pct            DOUBLE,
    mean_abs_harmonized_diff_pct DOUBLE,
    correlation                  DOUBLE,
    params                       VARCHAR   NOT NULL   -- JSON con la configuración usada
);

CREATE TABLE IF NOT EXISTS harmonization_factors (
    "month"  INTEGER PRIMARY KEY,  -- 1..12
    season   VARCHAR NOT NULL,     -- dry | wet
    modis    DOUBLE  NOT NULL,     -- celdas-día MODIS en los meses de traslape
    viirs    DOUBLE  NOT NULL,     -- celdas-día VIIRS en los meses de traslape
    f_month  DOUBLE,
    f_season DOUBLE,
    factor   DOUBLE,
    source   VARCHAR NOT NULL      -- month | season
);

CREATE TABLE IF NOT EXISTS harmonized_monthly (
    month_start DATE    PRIMARY KEY,
    modis       DOUBLE,             -- celdas-día MODIS Aqua
    viirs       DOUBLE,             -- celdas-día VIIRS (satélite de referencia)
    harmonized  DOUBLE,             -- equivalente MODIS
    source      VARCHAR NOT NULL,   -- modis | viirs_factor
    complete    BOOLEAN NOT NULL,
    median      DOUBLE,             -- mediana del mes en los años de referencia
    p25         DOUBLE,
    p75         DOUBLE,
    anomaly     DOUBLE,             -- (harmonized - median) / (p75 - p25)
    "level"     VARCHAR             -- very_low | low | normal | high | very_high
);

CREATE TABLE IF NOT EXISTS harmonization_validation (
    "year"              INTEGER PRIMARY KEY,
    months              BIGINT  NOT NULL,
    modis               DOUBLE  NOT NULL,
    viirs_raw           DOUBLE  NOT NULL,
    viirs_harmonized    DOUBLE,
    raw_diff_pct        DOUBLE,
    harmonized_diff_pct DOUBLE
);

CREATE TABLE IF NOT EXISTS harmonization_static_cells (
    cell_row     BIGINT NOT NULL,
    cell_col     BIGINT NOT NULL,
    latitude     DOUBLE NOT NULL,  -- centro de la celda
    longitude    DOUBLE NOT NULL,
    months       BIGINT NOT NULL,
    total_months BIGINT NOT NULL,
    PRIMARY KEY (cell_row, cell_col)
);
