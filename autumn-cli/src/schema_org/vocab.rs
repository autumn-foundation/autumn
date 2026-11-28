//! The schema.org types the CLI knows.
//!
//! This is a curated slice of the schema.org vocabulary: the types search
//! engines show rich results for, and the types their properties point at.
//! Each type lists its own properties. A type also has the properties of its
//! parents. The `required`, `required_any` and `recommended` lists follow
//! Google's structured data documentation for that rich result. They are
//! advice, not part of schema.org: schema.org itself requires nothing.
//!
//! The descriptions are short summaries, not the schema.org text. See
//! <https://schema.org/docs/full.html> for the whole vocabulary.

/// One property of a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Property {
    /// The property name, as it appears in JSON-LD (`datePublished`).
    pub name: &'static str,
    /// The expected types, in the order the CLI prefers them. Each is a
    /// primitive ([`PRIMITIVES`]) or a type in [`TYPES`].
    pub range: &'static [&'static str],
    /// A one-line summary.
    pub description: &'static str,
    /// The value is a list. A scaffold stores it one item per line.
    pub list: bool,
    /// The value is long text. A scaffold gives it a `Text` column.
    pub long: bool,
    /// A scaffold seeds this property as a node with columns of its own,
    /// though its type can be known by name. An event's `location` needs an
    /// address, not just a name.
    pub seed_nested: bool,
}

impl Property {
    const fn new(
        name: &'static str,
        range: &'static [&'static str],
        description: &'static str,
    ) -> Self {
        Self {
            name,
            range,
            description,
            list: false,
            long: false,
            seed_nested: false,
        }
    }

    const fn list(mut self) -> Self {
        self.list = true;
        self.long = true;
        self
    }

    const fn long(mut self) -> Self {
        self.long = true;
        self
    }

    const fn seed_nested(mut self) -> Self {
        self.seed_nested = true;
        self
    }
}

/// One schema.org type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaType {
    /// The type name (`Recipe`).
    pub name: &'static str,
    /// The direct parents. Most types have one. `LocalBusiness` has two.
    pub parents: &'static [&'static str],
    /// A one-line summary.
    pub description: &'static str,
    /// The type's own properties. Inherited ones are on the parents.
    pub properties: &'static [Property],
    /// Properties Google requires for the rich result.
    pub required: &'static [&'static str],
    /// Groups of properties: each group needs at least one of its members.
    pub required_any: &'static [&'static [&'static str]],
    /// Properties Google recommends for the rich result.
    pub recommended: &'static [&'static str],
    /// For an enumeration, its members (`InStock`). Empty for other types.
    pub members: &'static [&'static str],
    /// A value of this type is often known by its name alone, so a text
    /// column can stand for it: `{"@type": "Person", "name": "Ada"}`.
    pub nameable: bool,
    /// The Google Search documentation page for the rich result, if any.
    pub google_docs: Option<&'static str>,
}

impl SchemaType {
    const fn new(
        name: &'static str,
        parents: &'static [&'static str],
        description: &'static str,
        properties: &'static [Property],
    ) -> Self {
        Self {
            name,
            parents,
            description,
            properties,
            required: &[],
            required_any: &[],
            recommended: &[],
            members: &[],
            nameable: false,
            google_docs: None,
        }
    }

    const fn required(mut self, required: &'static [&'static str]) -> Self {
        self.required = required;
        self
    }

    const fn required_any(mut self, groups: &'static [&'static [&'static str]]) -> Self {
        self.required_any = groups;
        self
    }

    const fn recommended(mut self, recommended: &'static [&'static str]) -> Self {
        self.recommended = recommended;
        self
    }

    const fn members(mut self, members: &'static [&'static str]) -> Self {
        self.members = members;
        self
    }

    const fn nameable(mut self) -> Self {
        self.nameable = true;
        self
    }

    const fn google(mut self, page: &'static str) -> Self {
        self.google_docs = Some(page);
        self
    }

    /// Whether this type is an enumeration.
    #[must_use]
    pub const fn is_enumeration(&self) -> bool {
        !self.members.is_empty()
    }
}

/// The primitive data types. A property whose range holds one of these takes
/// a plain JSON value. `Duration`, `Energy` and `Mass` are schema.org types,
/// but their values are text (`PT30M`, `270 calories`, `12 g`), so they are
/// treated as text here.
pub const PRIMITIVES: &[&str] = &[
    "Text", "URL", "Date", "DateTime", "Time", "Number", "Integer", "Boolean", "Duration",
    "Energy", "Mass",
];

/// Whether `name` is in [`PRIMITIVES`].
#[must_use]
pub fn is_primitive(name: &str) -> bool {
    PRIMITIVES.contains(&name)
}

const fn p(name: &'static str, range: &'static [&'static str], d: &'static str) -> Property {
    Property::new(name, range, d)
}

const TEXT: &[&str] = &["Text"];
const URL: &[&str] = &["URL"];
const DATE_TIME: &[&str] = &["DateTime", "Date"];
const DATE: &[&str] = &["Date"];
const INTEGER: &[&str] = &["Integer"];
const NUMBER: &[&str] = &["Number", "Text"];
const BOOLEAN: &[&str] = &["Boolean"];
const DURATION: &[&str] = &["Duration"];
const PERSON_OR_ORG: &[&str] = &["Person", "Organization"];
const ORG_OR_PERSON: &[&str] = &["Organization", "Person"];
const IMAGE: &[&str] = &["URL", "ImageObject"];
const ADDRESS: &[&str] = &["PostalAddress", "Text"];

/// Every type the CLI knows, sorted by name.
pub const TYPES: &[SchemaType] = &[
    SchemaType::new(
        "AdministrativeArea",
        &["Place"],
        "A geographical region under one government: a state, a city.",
        &[],
    )
    .nameable(),
    SchemaType::new(
        "AggregateRating",
        &["Rating"],
        "The average rating of an item, from many ratings or reviews.",
        &[
            p("ratingCount", INTEGER, "How many ratings the average is of."),
            p("reviewCount", INTEGER, "How many reviews the average is of."),
            p("itemReviewed", &["Thing"], "The item being rated."),
        ],
    )
    .required(&["ratingValue"])
    .required_any(&[&["ratingCount", "reviewCount"]])
    .recommended(&["bestRating", "worstRating"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/review-snippet"),
    SchemaType::new(
        "Answer",
        &["Comment"],
        "An answer to a question.",
        &[],
    )
    .required(&["text"]),
    SchemaType::new(
        "Article",
        &["CreativeWork"],
        "An article: a news story, an essay, a report.",
        &[
            p("articleBody", TEXT, "The body of the article.").long(),
            p("articleSection", TEXT, "The section the article is in."),
            p("wordCount", INTEGER, "How many words the article has."),
        ],
    )
    .recommended(&["headline", "image", "datePublished", "dateModified", "author"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/article"),
    SchemaType::new(
        "BlogPosting",
        &["SocialMediaPosting"],
        "A post on a blog.",
        &[],
    )
    .recommended(&["headline", "image", "datePublished", "dateModified", "author"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/article"),
    SchemaType::new(
        "Book",
        &["CreativeWork"],
        "A book.",
        &[
            p("isbn", TEXT, "The ISBN."),
            p("numberOfPages", INTEGER, "How many pages the book has."),
            p("bookEdition", TEXT, "The edition."),
            p("bookFormat", &["BookFormatType"], "Hardcover, paperback, e-book, audiobook."),
            p("illustrator", &["Person"], "The illustrator."),
        ],
    )
    .recommended(&["name", "author", "isbn"]),
    SchemaType::new(
        "BookFormatType",
        &["Enumeration"],
        "The format of a book.",
        &[],
    )
    .members(&["AudiobookFormat", "EBook", "GraphicNovel", "Hardcover", "Paperback"]),
    SchemaType::new(
        "Brand",
        &["Intangible"],
        "A brand: the name a company sells a product or service under.",
        &[p("logo", IMAGE, "The brand's logo.")],
    )
    .recommended(&["name"])
    .nameable(),
    SchemaType::new(
        "BreadcrumbList",
        &["ItemList"],
        "The chain of pages that leads to the current page.",
        &[],
    )
    .required(&["itemListElement"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/breadcrumb"),
    SchemaType::new(
        "Comment",
        &["CreativeWork"],
        "A comment on an item.",
        &[
            p("upvoteCount", INTEGER, "How many up votes the comment has."),
            p("downvoteCount", INTEGER, "How many down votes the comment has."),
            p("parentItem", &["Comment", "CreativeWork"], "The item this comment answers."),
        ],
    ),
    SchemaType::new(
        "ContactPoint",
        &["StructuredValue"],
        "A way to contact a person or organization.",
        &[
            p("telephone", TEXT, "The phone number."),
            p("email", TEXT, "The email address."),
            p("contactType", TEXT, "What the contact is for: sales, support."),
            p("areaServed", &["Text", "Place"], "Where the contact serves."),
        ],
    ),
    SchemaType::new(
        "Country",
        &["AdministrativeArea"],
        "A country.",
        &[],
    )
    .nameable(),
    SchemaType::new(
        "Course",
        &["CreativeWork"],
        "A course of study.",
        &[
            p("courseCode", TEXT, "The course's code or number."),
            p("provider", ORG_OR_PERSON, "Who offers the course."),
            p(
                "educationalCredentialAwarded",
                &["Text", "URL"],
                "The credential a student gets.",
            ),
        ],
    )
    .required(&["name", "description"])
    .recommended(&["provider"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/course"),
    SchemaType::new(
        "CreativeWork",
        &["Thing"],
        "A creative work: a book, a film, a page, a post, a recipe.",
        &[
            p("headline", TEXT, "The headline."),
            p("author", PERSON_OR_ORG, "The author."),
            p("creator", PERSON_OR_ORG, "The creator."),
            p("publisher", ORG_OR_PERSON, "The publisher."),
            p("datePublished", DATE_TIME, "When the work was first published."),
            p("dateModified", DATE_TIME, "When the work last changed."),
            p("dateCreated", DATE_TIME, "When the work was created."),
            p("keywords", TEXT, "Keywords or tags, separated by commas."),
            p("inLanguage", TEXT, "The language, as an IETF BCP 47 tag (`en-GB`)."),
            p("text", TEXT, "The text of the work.").long(),
            p("abstract", TEXT, "A short summary.").long(),
            p("genre", &["Text", "URL"], "The genre."),
            p("about", &["Thing"], "What the work is about."),
            p("mainEntity", &["Thing"], "The main thing the work describes."),
            p("license", &["URL", "CreativeWork"], "The license."),
            p("thumbnailUrl", URL, "A thumbnail image."),
            p("creditText", TEXT, "The credit for the work."),
            p("copyrightNotice", TEXT, "The copyright notice."),
            p("copyrightYear", &["Number"], "The year of copyright."),
            p("isAccessibleForFree", BOOLEAN, "Whether the work is free to read."),
            p("aggregateRating", &["AggregateRating"], "The overall rating."),
            p("review", &["Review"], "A review of the work."),
            p("offers", &["Offer"], "An offer to sell or lend the work."),
            p("video", &["VideoObject"], "A video in the work."),
        ],
    ),
    SchemaType::new(
        "DataCatalog",
        &["CreativeWork"],
        "A catalog of datasets.",
        &[],
    )
    .nameable(),
    SchemaType::new(
        "DataDownload",
        &["MediaObject"],
        "A file a dataset can be downloaded as.",
        &[],
    )
    .required(&["contentUrl"])
    .recommended(&["encodingFormat"]),
    SchemaType::new(
        "Dataset",
        &["CreativeWork"],
        "A collection of data.",
        &[
            p("distribution", &["DataDownload"], "Where to download the data."),
            p("variableMeasured", TEXT, "What the data measures."),
            p("measurementTechnique", TEXT, "How the data was measured."),
            p("includedInDataCatalog", &["DataCatalog"], "The catalog the dataset is in."),
            p(
                "temporalCoverage",
                &["Text", "DateTime", "URL"],
                "The time span the data covers, as an ISO 8601 interval.",
            ),
            p("spatialCoverage", &["Place"], "The place the data covers."),
        ],
    )
    .required(&["name", "description"])
    .recommended(&["url", "sameAs", "license", "creator", "keywords"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/dataset"),
    SchemaType::new(
        "DiscussionForumPosting",
        &["SocialMediaPosting"],
        "A post in a discussion forum.",
        &[],
    )
    .required(&["author", "datePublished"])
    .required_any(&[&["text", "image", "video"]])
    .recommended(&["headline", "url", "dateModified"])
    .google(
        "https://developers.google.com/search/docs/appearance/structured-data/discussion-forum",
    ),
    SchemaType::new(
        "Enumeration",
        &["Intangible"],
        "A fixed set of values. Its subtypes list the members.",
        &[],
    ),
    SchemaType::new(
        "Event",
        &["Thing"],
        "An event at a time and place: a concert, a lecture, a festival.",
        &[
            p("startDate", DATE_TIME, "When the event starts."),
            p("endDate", DATE_TIME, "When the event ends."),
            p("doorTime", &["DateTime", "Time"], "When the doors open."),
            p(
                "location",
                &["Place", "PostalAddress", "Text", "VirtualLocation"],
                "Where the event is.",
            )
            .seed_nested(),
            p("organizer", ORG_OR_PERSON, "Who runs the event."),
            p("performer", PERSON_OR_ORG, "Who performs."),
            p("eventStatus", &["EventStatusType"], "Whether the event goes ahead."),
            p(
                "eventAttendanceMode",
                &["EventAttendanceModeEnumeration"],
                "In person, online, or both.",
            ),
            p("maximumAttendeeCapacity", INTEGER, "How many people can attend."),
            p("offers", &["Offer"], "Tickets."),
        ],
    )
    .required(&["name", "startDate", "location"])
    .recommended(&[
        "description",
        "endDate",
        "eventStatus",
        "image",
        "offers",
        "organizer",
        "performer",
    ])
    .google("https://developers.google.com/search/docs/appearance/structured-data/event"),
    SchemaType::new(
        "EventAttendanceModeEnumeration",
        &["Enumeration"],
        "How people attend an event.",
        &[],
    )
    .members(&[
        "MixedEventAttendanceMode",
        "OfflineEventAttendanceMode",
        "OnlineEventAttendanceMode",
    ]),
    SchemaType::new(
        "EventStatusType",
        &["Enumeration"],
        "Whether an event goes ahead.",
        &[],
    )
    .members(&[
        "EventCancelled",
        "EventMovedOnline",
        "EventPostponed",
        "EventRescheduled",
        "EventScheduled",
    ]),
    SchemaType::new(
        "FAQPage",
        &["WebPage"],
        "A page of frequently asked questions and their answers.",
        &[],
    )
    .required(&["mainEntity"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/faqpage"),
    SchemaType::new(
        "FoodEstablishment",
        &["LocalBusiness"],
        "A business that serves food: a restaurant, a bakery, a cafe.",
        &[
            p("servesCuisine", TEXT, "The cuisine."),
            p("hasMenu", &["URL", "Text"], "The menu."),
            p(
                "acceptsReservations",
                &["Boolean", "Text", "URL"],
                "Whether it takes reservations.",
            ),
        ],
    )
    .required(&["name", "address"])
    .recommended(&["telephone", "url", "priceRange", "servesCuisine", "geo", "image"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/local-business"),
    SchemaType::new(
        "GeoCoordinates",
        &["StructuredValue"],
        "A latitude and a longitude.",
        &[
            p("latitude", NUMBER, "The latitude, in decimal degrees."),
            p("longitude", NUMBER, "The longitude, in decimal degrees."),
        ],
    )
    .required(&["latitude", "longitude"]),
    SchemaType::new(
        "HowTo",
        &["CreativeWork"],
        "Steps to get something done.",
        &[
            p("step", &["Text", "HowToStep"], "The steps, in order.").list(),
            p("totalTime", DURATION, "How long it takes, as an ISO 8601 duration."),
            p("prepTime", DURATION, "How long the preparation takes."),
            p("performTime", DURATION, "How long the work itself takes."),
            p("supply", TEXT, "A thing used up along the way.").list(),
            p("tool", TEXT, "A tool needed.").list(),
            p("estimatedCost", &["Text", "MonetaryAmount"], "What it costs."),
            p("yield", TEXT, "What it makes."),
        ],
    ),
    SchemaType::new(
        "HowToStep",
        &["ListItem", "CreativeWork"],
        "One step of a how-to or a recipe.",
        &[],
    )
    .required(&["text"]),
    SchemaType::new(
        "ImageObject",
        &["MediaObject"],
        "An image file.",
        &[p("caption", TEXT, "The caption.")],
    )
    .required(&["contentUrl"])
    .recommended(&["creator", "creditText", "copyrightNotice", "license"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/image-license-metadata"),
    SchemaType::new(
        "Intangible",
        &["Thing"],
        "A thing that is not physical: an offer, a rating, a quantity.",
        &[],
    ),
    SchemaType::new(
        "ItemAvailability",
        &["Enumeration"],
        "Whether an item can be bought.",
        &[],
    )
    .members(&[
        "BackOrder",
        "Discontinued",
        "InStock",
        "InStoreOnly",
        "LimitedAvailability",
        "OnlineOnly",
        "OutOfStock",
        "PreOrder",
        "PreSale",
        "SoldOut",
    ]),
    SchemaType::new(
        "ItemList",
        &["Intangible"],
        "An ordered list of items.",
        &[
            p(
                "itemListElement",
                &["ListItem", "Thing", "Text"],
                "The items, in order.",
            ),
            p("numberOfItems", INTEGER, "How many items the list has."),
        ],
    ),
    SchemaType::new(
        "JobPosting",
        &["Intangible"],
        "An open job.",
        &[
            p("title", TEXT, "The job title."),
            p("datePosted", DATE_TIME, "When the job was posted."),
            p("validThrough", DATE_TIME, "When the posting expires."),
            p("hiringOrganization", ORG_OR_PERSON, "The employer."),
            p("jobLocation", &["Place"], "Where the work is.").seed_nested(),
            p("jobLocationType", TEXT, "`TELECOMMUTE` for a remote job."),
            p(
                "applicantLocationRequirements",
                &["AdministrativeArea"],
                "Where a remote applicant must live.",
            ),
            p(
                "employmentType",
                TEXT,
                "FULL_TIME, PART_TIME, CONTRACTOR, TEMPORARY, INTERN, and others.",
            ),
            p("baseSalary", &["MonetaryAmount", "Number"], "The base pay."),
            p("directApply", BOOLEAN, "Whether one can apply on this page."),
        ],
    )
    .required(&["title", "description", "datePosted", "hiringOrganization", "jobLocation"])
    .recommended(&["baseSalary", "employmentType", "validThrough", "identifier", "directApply"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/job-posting"),
    SchemaType::new(
        "ListItem",
        &["Intangible"],
        "One item in a list.",
        &[
            p("position", &["Integer", "Text"], "The item's place in the list, from 1."),
            p("item", &["Thing", "URL"], "The item itself."),
        ],
    )
    .required(&["position"])
    .recommended(&["name", "item"]),
    SchemaType::new(
        "LocalBusiness",
        &["Organization", "Place"],
        "A business with a physical place: a shop, an office, a branch.",
        &[
            p("openingHours", TEXT, "When it is open, like `Mo-Fr 09:00-17:00`."),
            p("priceRange", TEXT, "How expensive it is, like `$$`."),
            p("currenciesAccepted", TEXT, "The currencies it takes."),
            p("paymentAccepted", TEXT, "The payment methods it takes."),
        ],
    )
    .required(&["name", "address"])
    .recommended(&["telephone", "url", "priceRange", "openingHours", "geo", "image"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/local-business"),
    SchemaType::new(
        "MediaObject",
        &["CreativeWork"],
        "A media file: an image, a video, an audio file.",
        &[
            p("contentUrl", URL, "The file itself."),
            p("embedUrl", URL, "A player for the file."),
            p("encodingFormat", &["Text", "URL"], "The media type, like `video/mp4`."),
            p("duration", DURATION, "How long it plays, as an ISO 8601 duration."),
            p("uploadDate", DATE_TIME, "When it was uploaded."),
        ],
    ),
    SchemaType::new(
        "MonetaryAmount",
        &["StructuredValue"],
        "An amount of money.",
        &[
            p("currency", TEXT, "The currency, as an ISO 4217 code."),
            p("value", &["Number"], "The amount."),
            p("minValue", &["Number"], "The low end of a range."),
            p("maxValue", &["Number"], "The high end of a range."),
        ],
    )
    .required(&["currency", "value"]),
    SchemaType::new(
        "Movie",
        &["CreativeWork"],
        "A film.",
        &[
            p("director", &["Person"], "The director."),
            p("actor", &["Person"], "An actor."),
            p("duration", DURATION, "How long it runs, as an ISO 8601 duration."),
            p("countryOfOrigin", &["Country"], "Where it was made."),
        ],
    )
    .required(&["name", "image"])
    .recommended(&["dateCreated", "director", "aggregateRating", "review"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/movie"),
    SchemaType::new(
        "NewsArticle",
        &["Article"],
        "A news story.",
        &[
            p("dateline", TEXT, "Where and when the story was written."),
            p("printSection", TEXT, "The print section it ran in."),
            p("printPage", TEXT, "The print page it ran on."),
        ],
    )
    .recommended(&["headline", "image", "datePublished", "dateModified", "author"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/article"),
    SchemaType::new(
        "NutritionInformation",
        &["StructuredValue"],
        "What a serving of food contains.",
        &[
            p("calories", &["Energy"], "The energy, like `270 calories`."),
            p("servingSize", TEXT, "The serving size."),
            p("fatContent", &["Mass"], "Fat, like `12 g`."),
            p("proteinContent", &["Mass"], "Protein."),
            p("carbohydrateContent", &["Mass"], "Carbohydrate."),
            p("sugarContent", &["Mass"], "Sugar."),
            p("sodiumContent", &["Mass"], "Sodium, like `300 mg`."),
        ],
    )
    .recommended(&["calories"]),
    SchemaType::new(
        "Offer",
        &["Intangible"],
        "An offer to sell, rent or lend an item, at a price.",
        &[
            p("price", NUMBER, "The price, with `.` as the decimal point."),
            p("priceCurrency", TEXT, "The currency, as an ISO 4217 code."),
            p("availability", &["ItemAvailability"], "Whether the item can be bought."),
            p("priceValidUntil", DATE, "The last day of the price."),
            p("validFrom", DATE_TIME, "When the offer starts."),
            p("itemCondition", &["OfferItemCondition"], "New, used, refurbished."),
            p("seller", ORG_OR_PERSON, "Who sells."),
        ],
    )
    .required(&["price", "priceCurrency"])
    .recommended(&["availability", "priceValidUntil", "url"]),
    SchemaType::new(
        "OfferItemCondition",
        &["Enumeration"],
        "The condition of an item for sale.",
        &[],
    )
    .members(&[
        "DamagedCondition",
        "NewCondition",
        "RefurbishedCondition",
        "UsedCondition",
    ]),
    SchemaType::new(
        "Organization",
        &["Thing"],
        "An organization: a company, a school, a club.",
        &[
            p("legalName", TEXT, "The registered name."),
            p("logo", IMAGE, "The logo."),
            p("email", TEXT, "The email address."),
            p("telephone", TEXT, "The phone number."),
            p("address", ADDRESS, "The postal address."),
            p("foundingDate", DATE, "When it was founded."),
            p("founder", &["Person"], "Who founded it."),
            p("contactPoint", &["ContactPoint"], "A way to contact it."),
            p("taxID", TEXT, "The tax id."),
            p("vatID", TEXT, "The VAT id."),
        ],
    )
    .recommended(&["name", "url", "logo", "address", "email", "telephone", "sameAs"])
    .nameable()
    .google("https://developers.google.com/search/docs/appearance/structured-data/organization"),
    SchemaType::new(
        "Person",
        &["Thing"],
        "A person.",
        &[
            p("givenName", TEXT, "The first name."),
            p("familyName", TEXT, "The last name."),
            p("email", TEXT, "The email address."),
            p("telephone", TEXT, "The phone number."),
            p("jobTitle", TEXT, "The job title."),
            p("birthDate", DATE, "The date of birth."),
            p("worksFor", &["Organization"], "The employer."),
            p("affiliation", &["Organization"], "An organization the person belongs to."),
            p("address", ADDRESS, "The postal address."),
        ],
    )
    .required(&["name"])
    .recommended(&["url", "image", "sameAs", "jobTitle"])
    .nameable()
    .google("https://developers.google.com/search/docs/appearance/structured-data/profile-page"),
    SchemaType::new(
        "Place",
        &["Thing"],
        "A place: a venue, a building, a city.",
        &[
            p("address", ADDRESS, "The postal address."),
            p("geo", &["GeoCoordinates"], "The coordinates."),
            p("telephone", TEXT, "The phone number."),
            p("latitude", NUMBER, "The latitude, in decimal degrees."),
            p("longitude", NUMBER, "The longitude, in decimal degrees."),
            p("hasMap", URL, "A map of the place."),
        ],
    )
    .recommended(&["name", "address"])
    .nameable(),
    SchemaType::new(
        "PostalAddress",
        &["ContactPoint"],
        "A postal address.",
        &[
            p("streetAddress", TEXT, "The street and number."),
            p("addressLocality", TEXT, "The town or city."),
            p("addressRegion", TEXT, "The state or region."),
            p("postalCode", TEXT, "The postal code."),
            p("addressCountry", &["Text", "Country"], "The country, as an ISO 3166-1 code."),
            p("postOfficeBoxNumber", TEXT, "The PO box."),
        ],
    )
    .recommended(&[
        "streetAddress",
        "addressLocality",
        "addressRegion",
        "postalCode",
        "addressCountry",
    ]),
    SchemaType::new(
        "Product",
        &["Thing"],
        "A product for sale.",
        &[
            p("brand", &["Brand", "Organization"], "The brand."),
            p("sku", TEXT, "The merchant's stock keeping unit."),
            p("gtin", TEXT, "The GTIN: a UPC, EAN or ISBN barcode number."),
            p("mpn", TEXT, "The manufacturer part number."),
            p("color", TEXT, "The color."),
            p("material", TEXT, "What it is made of."),
            p("category", TEXT, "The category."),
            p("offers", &["Offer"], "An offer to sell it."),
            p("aggregateRating", &["AggregateRating"], "The overall rating."),
            p("review", &["Review"], "A review."),
        ],
    )
    .required(&["name"])
    .required_any(&[&["offers", "review", "aggregateRating"]])
    .recommended(&["image", "description", "sku", "brand", "gtin", "mpn"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/product-snippet"),
    SchemaType::new(
        "PropertyValue",
        &["StructuredValue"],
        "A named value: an identifier with its scheme, a measured value.",
        &[
            p("propertyID", &["Text", "URL"], "What the value is of."),
            p("value", &["Text", "Number", "Boolean"], "The value."),
        ],
    ),
    SchemaType::new(
        "QuantitativeValue",
        &["StructuredValue"],
        "A measured value with a unit.",
        &[
            p("value", &["Number", "Text", "Boolean"], "The value."),
            p("unitText", TEXT, "The unit, as text."),
            p("unitCode", &["Text", "URL"], "The unit, as a UN/CEFACT code."),
            p("minValue", &["Number"], "The low end of a range."),
            p("maxValue", &["Number"], "The high end of a range."),
        ],
    ),
    SchemaType::new(
        "Question",
        &["Comment"],
        "A question.",
        &[
            p("acceptedAnswer", &["Answer", "ItemList"], "The best answer."),
            p("suggestedAnswer", &["Answer", "ItemList"], "Another answer."),
            p("answerCount", INTEGER, "How many answers there are."),
        ],
    )
    .required(&["name"])
    .required_any(&[&["acceptedAnswer", "suggestedAnswer"]])
    .google("https://developers.google.com/search/docs/appearance/structured-data/qapage"),
    SchemaType::new(
        "Rating",
        &["Intangible"],
        "One rating on a scale.",
        &[
            p("ratingValue", NUMBER, "The rating."),
            p("bestRating", NUMBER, "The top of the scale. Default 5."),
            p("worstRating", NUMBER, "The bottom of the scale. Default 1."),
        ],
    )
    .required(&["ratingValue"]),
    SchemaType::new(
        "Recipe",
        &["HowTo"],
        "A recipe: what goes in, the steps, and what comes out.",
        &[
            p("recipeIngredient", TEXT, "One ingredient and its amount.").list(),
            p(
                "recipeInstructions",
                &["Text", "HowToStep", "ItemList"],
                "The steps, in order.",
            )
            .list(),
            p("recipeYield", &["Text", "QuantitativeValue"], "How much it makes: `4 servings`."),
            p("recipeCategory", TEXT, "The kind of dish: dinner, dessert."),
            p("recipeCuisine", TEXT, "The cuisine: French, Thai."),
            p("cookTime", DURATION, "How long it cooks, as an ISO 8601 duration."),
            p("nutrition", &["NutritionInformation"], "Nutrition per serving."),
            p("suitableForDiet", &["RestrictedDiet"], "A diet the recipe fits."),
        ],
    )
    .required(&["name", "image"])
    .recommended(&[
        "author",
        "datePublished",
        "description",
        "prepTime",
        "cookTime",
        "totalTime",
        "keywords",
        "recipeYield",
        "recipeCategory",
        "recipeCuisine",
        "recipeIngredient",
        "recipeInstructions",
        "nutrition",
    ])
    .google("https://developers.google.com/search/docs/appearance/structured-data/recipe"),
    SchemaType::new(
        "Restaurant",
        &["FoodEstablishment"],
        "A restaurant.",
        &[],
    )
    .required(&["name", "address"])
    .recommended(&["telephone", "url", "priceRange", "servesCuisine", "geo", "image"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/local-business"),
    SchemaType::new(
        "RestrictedDiet",
        &["Enumeration"],
        "A diet: vegan, gluten free, halal.",
        &[],
    )
    .members(&[
        "DiabeticDiet",
        "GlutenFreeDiet",
        "HalalDiet",
        "HinduDiet",
        "KosherDiet",
        "LowCalorieDiet",
        "LowFatDiet",
        "LowLactoseDiet",
        "LowSaltDiet",
        "VeganDiet",
        "VegetarianDiet",
    ]),
    SchemaType::new(
        "Review",
        &["CreativeWork"],
        "A review of an item.",
        &[
            p("itemReviewed", &["Thing"], "The item being reviewed."),
            p("reviewRating", &["Rating"], "The rating the reviewer gave."),
            p("reviewBody", TEXT, "The text of the review.").long(),
        ],
    )
    .required(&["author", "itemReviewed", "reviewRating"])
    .recommended(&["datePublished", "reviewBody"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/review-snippet"),
    SchemaType::new(
        "Service",
        &["Intangible"],
        "A service: work done for a customer.",
        &[
            p("provider", ORG_OR_PERSON, "Who provides the service."),
            p("serviceType", TEXT, "The kind of service."),
            p("areaServed", &["Text", "Place"], "Where the service is offered."),
            p("brand", &["Brand", "Organization"], "The brand."),
            p("offers", &["Offer"], "An offer for the service."),
        ],
    )
    .recommended(&["name", "description", "provider", "serviceType", "areaServed"]),
    SchemaType::new(
        "SocialMediaPosting",
        &["Article"],
        "A post on a social site, a forum or a blog.",
        &[p("sharedContent", &["CreativeWork"], "A work the post shares.")],
    ),
    SchemaType::new(
        "SoftwareApplication",
        &["CreativeWork"],
        "A program or an app.",
        &[
            p("applicationCategory", &["Text", "URL"], "The kind of app, like `GameApplication`."),
            p("operatingSystem", TEXT, "The operating systems it runs on."),
            p("softwareVersion", TEXT, "The version."),
            p("downloadUrl", URL, "Where to download it."),
        ],
    )
    .required(&["name", "offers"])
    .required_any(&[&["aggregateRating", "review"]])
    .recommended(&["applicationCategory", "operatingSystem"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/software-app"),
    SchemaType::new(
        "Store",
        &["LocalBusiness"],
        "A shop.",
        &[],
    )
    .required(&["name", "address"])
    .recommended(&["telephone", "url", "priceRange", "openingHours", "geo", "image"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/local-business"),
    SchemaType::new(
        "StructuredValue",
        &["Intangible"],
        "A value made of several parts: an address, coordinates, an amount.",
        &[],
    ),
    SchemaType::new(
        "TechArticle",
        &["Article"],
        "A technical article: a how-to, a reference page.",
        &[
            p("proficiencyLevel", TEXT, "Beginner or Expert."),
            p("dependencies", TEXT, "What the reader needs first."),
        ],
    )
    .recommended(&["headline", "image", "datePublished", "dateModified", "author"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/article"),
    SchemaType::new(
        "Thing",
        &[],
        "The most general type. Every other type is a kind of Thing.",
        &[
            p("name", TEXT, "The name."),
            p("alternateName", TEXT, "Another name."),
            p("description", TEXT, "A description.").long(),
            p("url", URL, "The URL of the item."),
            p("image", IMAGE, "An image of the item."),
            p("identifier", &["Text", "URL", "PropertyValue"], "An id: an ISBN, a GTIN, a slug."),
            p("sameAs", URL, "A page that unambiguously names the item.").list(),
        ],
    )
    .nameable(),
    SchemaType::new(
        "VideoObject",
        &["MediaObject"],
        "A video file.",
        &[
            p("caption", TEXT, "The caption."),
            p("transcript", TEXT, "The transcript.").long(),
        ],
    )
    .required(&["name", "thumbnailUrl", "uploadDate"])
    .recommended(&["description", "contentUrl", "embedUrl", "duration"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/video"),
    SchemaType::new(
        "VirtualLocation",
        &["Intangible"],
        "An online place: the URL of a stream or a meeting.",
        &[],
    )
    .required(&["url"]),
    SchemaType::new(
        "WebApplication",
        &["SoftwareApplication"],
        "An app that runs in a browser.",
        &[p("browserRequirements", TEXT, "What the browser needs.")],
    )
    .required(&["name", "offers"])
    .required_any(&[&["aggregateRating", "review"]])
    .recommended(&["applicationCategory", "operatingSystem"])
    .google("https://developers.google.com/search/docs/appearance/structured-data/software-app"),
    SchemaType::new(
        "WebPage",
        &["CreativeWork"],
        "A web page.",
        &[
            p("breadcrumb", &["BreadcrumbList", "Text"], "The breadcrumb trail to the page."),
            p("lastReviewed", DATE, "When the content was last checked."),
            p("primaryImageOfPage", &["ImageObject"], "The page's main image."),
        ],
    ),
    SchemaType::new(
        "WebSite",
        &["CreativeWork"],
        "A web site: a set of pages under one domain.",
        &[],
    )
    .recommended(&["name", "url", "alternateName"])
    .google("https://developers.google.com/search/docs/appearance/site-names"),
];

/// The type named `name`, matched exactly (`Recipe`).
#[must_use]
pub fn find(name: &str) -> Option<&'static SchemaType> {
    TYPES.iter().find(|t| t.name == name)
}

/// The type named `name`, ignoring ASCII case and a `schema:` or
/// `https://schema.org/` prefix (`recipe`, `schema:Recipe`).
#[must_use]
pub fn lookup(name: &str) -> Option<&'static SchemaType> {
    let bare = strip_schema_prefix(name);
    find(bare).or_else(|| TYPES.iter().find(|t| t.name.eq_ignore_ascii_case(bare)))
}

/// `name` without a `schema:`, `http://schema.org/` or `https://schema.org/`
/// prefix.
#[must_use]
pub fn strip_schema_prefix(name: &str) -> &str {
    ["https://schema.org/", "http://schema.org/", "schema:"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name)
}

/// `t` and every type it inherits from, nearest first, each once.
#[must_use]
pub fn ancestry(t: &'static SchemaType) -> Vec<&'static SchemaType> {
    let mut out: Vec<&'static SchemaType> = Vec::new();
    let mut queue = vec![t];
    while !queue.is_empty() {
        let next = queue.remove(0);
        if out.iter().any(|seen| seen.name == next.name) {
            continue;
        }
        out.push(next);
        queue.extend(next.parents.iter().filter_map(|parent| find(parent)));
    }
    out
}

/// Whether `t` is `ancestor` or inherits from it.
#[must_use]
pub fn is_a(t: &'static SchemaType, ancestor: &str) -> bool {
    ancestry(t).iter().any(|a| a.name == ancestor)
}

/// A property and the type that declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedProperty {
    /// The property.
    pub property: &'static Property,
    /// The type that declares it: `t` itself or an ancestor.
    pub declared_on: &'static str,
}

/// Every property of `t`, its own first, then inherited ones, each name once.
#[must_use]
pub fn properties(t: &'static SchemaType) -> Vec<ResolvedProperty> {
    let mut out: Vec<ResolvedProperty> = Vec::new();
    for a in ancestry(t) {
        for property in a.properties {
            if !out.iter().any(|r| r.property.name == property.name) {
                out.push(ResolvedProperty {
                    property,
                    declared_on: a.name,
                });
            }
        }
    }
    out
}

/// The property `name` of `t`, own or inherited.
#[must_use]
pub fn property(t: &'static SchemaType, name: &str) -> Option<ResolvedProperty> {
    properties(t).into_iter().find(|r| r.property.name == name)
}

/// How much Google asks for a property on a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Neither required nor recommended.
    Optional,
    /// Recommended.
    Recommended,
    /// Required, alone or as one of a group.
    Required,
}

impl Level {
    /// The lowercase name: `required`, `recommended`, `optional`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Recommended => "recommended",
            Self::Optional => "optional",
        }
    }
}

/// How much Google asks for `name` on `t`.
#[must_use]
pub fn level(t: &SchemaType, name: &str) -> Level {
    if t.required.contains(&name) || t.required_any.iter().any(|g| g.contains(&name)) {
        Level::Required
    } else if t.recommended.contains(&name) {
        Level::Recommended
    } else {
        Level::Optional
    }
}

/// The closest type name to `name`, for a "did you mean" hint.
#[must_use]
pub fn suggest_type(name: &str) -> Option<&'static str> {
    closest(name, TYPES.iter().map(|t| t.name))
}

/// The closest property of `t` to `name`, for a "did you mean" hint.
#[must_use]
pub fn suggest_property(t: &'static SchemaType, name: &str) -> Option<&'static str> {
    closest(name, properties(t).into_iter().map(|r| r.property.name))
}

fn closest(name: &str, candidates: impl Iterator<Item = &'static str>) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    candidates
        .map(|c| (edit_distance(&lower, &c.to_ascii_lowercase()), c))
        .filter(|(d, c)| *d <= (c.len() / 3).max(2))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Levenshtein distance over chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_sorted_and_unique() {
        for pair in TYPES.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "{} then {}",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn every_parent_and_range_entry_resolves() {
        for t in TYPES {
            for parent in t.parents {
                assert!(
                    find(parent).is_some(),
                    "{}: unknown parent {parent}",
                    t.name
                );
            }
            for prop in t.properties {
                assert!(
                    !prop.range.is_empty(),
                    "{}.{}: empty range",
                    t.name,
                    prop.name
                );
                for r in prop.range {
                    assert!(
                        is_primitive(r) || find(r).is_some(),
                        "{}.{}: unknown range type {r}",
                        t.name,
                        prop.name
                    );
                }
            }
        }
    }

    #[test]
    fn every_type_descends_from_thing() {
        for t in TYPES {
            assert!(is_a(t, "Thing"), "{} does not reach Thing", t.name);
        }
    }

    #[test]
    fn google_lists_name_real_properties() {
        for t in TYPES {
            let names: Vec<&str> = t
                .required
                .iter()
                .chain(t.recommended)
                .chain(t.required_any.iter().flat_map(|g| g.iter()))
                .copied()
                .collect();
            for name in names {
                assert!(
                    property(t, name).is_some(),
                    "{}: listed property {name} is not a property of the type",
                    t.name
                );
            }
        }
    }

    #[test]
    fn a_property_is_not_both_required_and_recommended() {
        for t in TYPES {
            for name in t.recommended {
                assert_eq!(level(t, name), Level::Recommended, "{}.{name}", t.name);
            }
        }
    }

    #[test]
    fn enumerations_descend_from_enumeration() {
        for t in TYPES.iter().filter(|t| t.is_enumeration()) {
            assert!(is_a(t, "Enumeration"), "{}", t.name);
        }
    }

    #[test]
    fn lookup_ignores_case_and_prefix() {
        assert_eq!(lookup("recipe").map(|t| t.name), Some("Recipe"));
        assert_eq!(lookup("schema:Product").map(|t| t.name), Some("Product"));
        assert_eq!(
            lookup("https://schema.org/Event").map(|t| t.name),
            Some("Event")
        );
        assert_eq!(lookup("Spaceship"), None);
    }

    #[test]
    fn inherited_properties_come_after_own_ones() {
        let recipe = find("Recipe").expect("Recipe");
        let props = properties(recipe);
        assert_eq!(props[0].property.name, "recipeIngredient");
        let name = property(recipe, "name").expect("name");
        assert_eq!(name.declared_on, "Thing");
        let total = property(recipe, "totalTime").expect("totalTime");
        assert_eq!(total.declared_on, "HowTo");
    }

    #[test]
    fn multiple_parents_are_all_inherited() {
        let business = find("LocalBusiness").expect("LocalBusiness");
        assert_eq!(
            property(business, "logo").map(|r| r.declared_on),
            Some("Organization")
        );
        assert_eq!(
            property(business, "geo").map(|r| r.declared_on),
            Some("Place")
        );
        // `address` is on both parents. The first parent wins.
        assert_eq!(
            property(business, "address").map(|r| r.declared_on),
            Some("Organization")
        );
    }

    #[test]
    fn levels_follow_the_google_lists() {
        let product = find("Product").expect("Product");
        assert_eq!(level(product, "name"), Level::Required);
        assert_eq!(level(product, "offers"), Level::Required);
        assert_eq!(level(product, "sku"), Level::Recommended);
        assert_eq!(level(product, "color"), Level::Optional);
    }

    #[test]
    fn suggestions_catch_typos() {
        assert_eq!(suggest_type("Recipie"), Some("Recipe"));
        assert_eq!(suggest_type("Zzzzzzzz"), None);
        let recipe = find("Recipe").expect("Recipe");
        assert_eq!(
            suggest_property(recipe, "recipeIngredients"),
            Some("recipeIngredient")
        );
        assert_eq!(
            suggest_property(recipe, "datepublished"),
            Some("datePublished")
        );
    }
}
